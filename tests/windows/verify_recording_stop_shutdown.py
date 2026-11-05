"""Close/EOF must interrupt a recorder already inside graceful stop."""
import http.server
import json
import os
from pathlib import Path
import tempfile
import threading
import time
import verify_mcp_browser_session as fixture
from verify_recording_stop import wait

def main():
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), fixture.Site)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        for eof in (False, True):
            with tempfile.TemporaryDirectory(prefix="aad-stop-shutdown-") as root:
                gate = Path(root)/"body"
                client = fixture.Client(dict(os.environ,AAD_RECORDINGS_DIR=root,AAD_TEST_BODY_GATE=str(gate),AAD_CONTROL_LEASE_DIR=root+"/leases"))
                try:
                    opened,_=client.tool("browser_open",{"browser":"edge","headless":True,"url":f"http://127.0.0.1:{server.server_port}/login","record":{"body_limit":4096}})
                    out=Path(opened["recording"]["out"])
                    page=fixture.observe(client,"Fixture login")
                    button=fixture.node(page,lambda n:n["name"]=="Sign in")
                    client.tool("control_act",{"target":button["target"],"action":{"type":"click"}})
                    wait(gate)
                    client.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":900,"method":"tools/call","params":{"name":"browser_record_stop","arguments":{}}})+"\n")
                    client.process.stdin.flush()
                    wait(Path(str(gate)+".stop"))
                    started=time.monotonic()
                    if eof:
                        client.process.stdin.close()
                        client.process.wait(timeout=1.5)
                    else:
                        client.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":901,"method":"tools/call","params":{"name":"control_close","arguments":{}}})+"\n")
                        client.process.stdin.flush()
                        replies={}
                        while len(replies)<2:
                            answer=client.receive(started+1.5)
                            replies[answer["id"]]=answer
                        summary=json.loads(replies[900]["result"]["content"][0]["text"])["summary"]
                        assert summary["status"]=="incomplete" and "shutdown" in summary["error"],summary
                    assert time.monotonic()-started<1.5
                    manifest=json.loads((out/"manifest.json").read_text())
                    assert manifest["status"]=="incomplete" and "shutdown" in manifest["error"],manifest
                    snapshot={str(p):p.read_bytes() for p in out.rglob("*") if p.is_file()}
                    time.sleep(.1)
                    assert snapshot=={str(p):p.read_bytes() for p in out.rglob("*") if p.is_file()}
                    print("PASS", "EOF" if eof else "control_close")
                finally:
                    client.close()
    finally:
        server.shutdown()

if __name__=="__main__": main()
