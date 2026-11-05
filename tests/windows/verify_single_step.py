"""Loopback-only asynchronous effect verification; isolated browser profile."""
import http.server
import os
import tempfile
import threading
import subprocess
import json

import verify_mcp_browser_session as fixture


def main(use_scope=True):
    fixture.LOGIN = """<!doctype html><title>Fixture login</title>
    <div id="noise"></div><h1>Done 1</h1><section id="work"><h1>Waiting</h1><button onclick="window.clicks=(window.clicks||0)+1;
    setTimeout(()=>document.querySelector('#work h1').textContent='Done '+window.clicks,400)">Run</button></section><script>noise.innerHTML="<p>noise</p>".repeat(2100)</script>"""
    if not use_scope:
        fixture.LOGIN=fixture.LOGIN.replace("<h1>Done 1</h1>","").replace("repeat(2100)","repeat(1500)")
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), fixture.Site)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="aad-verify-") as root:
            client = fixture.Client(dict(os.environ, AAD_RECORDINGS_DIR=root))
            try:
                opened, _ = client.tool("browser_open", {
                    "browser": os.environ.get("AAD_TEST_BROWSER", "edge"),
                    "url": f"http://127.0.0.1:{server.server_address[1]}/login",
                    "headless": True,
                })
                resource=fixture.observe(client, "Fixture login")["resource_id"]
                def scoped():
                    value, _ = client.tool("control_observe", {"resource_id":resource,
                        "max_nodes":2000,"view":({"mode":"nodes","scope_id":"work"} if use_scope else {"mode":"nodes"})})
                    value=value["data"]
                    assert not value["truncated"],value
                    if not use_scope: assert 1000 < len(value["nodes"]) <= 2000, len(value["nodes"])
                    return value
                scoped()
                for expected, verified, count in [("Done 1", True, 1), ("Never", False, 2)]:
                    result = subprocess.run([fixture.AAD, "do", "click", resource,
                        *(["--scope-id", "work"] if use_scope else []), "--max-nodes", "2000", "--name", "Run",
                        "--verify", json.dumps({"name":expected})],
                        env=dict(os.environ, AAD_CDP_PORT=str(opened["port"])),
                        capture_output=True, text=True, timeout=45)
                    envelope=json.loads(result.stdout)
                    assert "result" in envelope, envelope
                    answer=envelope["result"]
                    assert answer["submitted"] and answer["verified"] is verified, answer
                    if not verified:
                        assert answer["verification"]=="timeout_unconfirmed",answer
                        assert answer["verification_error"]["code"] in ["CONTROL.WAIT_TIMEOUT","CONTROL.TIMEOUT"],answer
                    assert any(n["name"]==f"Done {count}" for n in scoped()["nodes"])
                for expected, verified, count in [("Done 3", True, 3), ("Never", False, 4)]:
                    before = scoped()
                    target = next(n["target"] for n in before["nodes"] if n["name"] == "Run")
                    answer, _ = client.tool("control_act", {
                        "target": target, "action": {"type": "click"},
                        "verify": {"name": expected}, "timeout_ms": 1500,
                    })
                    assert answer["data"]["submitted"] is True, answer
                    assert answer["data"]["verified"] is verified, answer
                    if not verified:
                        assert answer["data"]["verification"]=="timeout_unconfirmed",answer
                        assert answer["data"]["verification_error"]["code"] in ["CONTROL.WAIT_TIMEOUT","CONTROL.TIMEOUT"],answer
                        assert len(answer["data"]["diagnostics"]["page"]["nodes"])<=20, answer
                        assert "summary" in answer["data"]["diagnostics"]["network"], answer
                    after = scoped()
                    assert any(n["name"] == f"Done {count}" for n in after["nodes"]), after
            finally:
                client.close()
    finally:
        server.shutdown()
        server.server_close()
    print("PASS: asynchronous success and timeout dispatch exactly once")


if __name__ == "__main__":
    main()
    main(False)
