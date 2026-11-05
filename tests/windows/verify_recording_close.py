"""Session close revokes recording, including startup, without closing an attachment."""
import http.server
import json
import os
from pathlib import Path
import tempfile
import threading
import time
import verify_mcp_browser_session as fixture

def main():
    fixture.LOGIN=fixture.LOGIN.replace("location.href='/home'",
        "document.querySelector('h1').textContent='After close';fetch('/after-close')")
    server=http.server.ThreadingHTTPServer(("127.0.0.1",0),fixture.Site)
    threading.Thread(target=server.serve_forever,daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="aad-record-close-") as root:
            env=dict(os.environ,AAD_RECORDINGS_DIR=root,AAD_CONTROL_LEASE_DIR=os.path.join(root,"leases"))
            owner=fixture.Client(env)
            try:
                opened,_=owner.tool("browser_open",{"browser":os.environ.get("AAD_TEST_BROWSER","edge"),
                    "headless":True,"url":f"http://127.0.0.1:{server.server_port}/login"})
                for held in [False,True]:
                    marker=Path(root)/"recorder-start"
                    attached=fixture.Client(dict(env,**({"AAD_TEST_RECORDER_START_BARRIER":str(marker)} if held else {})),opened["port"])
                    try:
                        args={"page_id":opened["page_id"],"record":{}}
                        if held:
                            attached.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":100,"method":"tools/call",
                                "params":{"name":"browser_record_start","arguments":args}})+"\n")
                            attached.process.stdin.flush()
                            deadline=time.monotonic()+5
                            while not marker.exists():
                                assert time.monotonic()<deadline,"startup barrier not entered"
                                time.sleep(.01)
                            attached.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":101,"method":"tools/call",
                                "params":{"name":"control_close","arguments":{}}})+"\n")
                            attached.process.stdin.flush()
                            replies={}
                            while len(replies)<2:
                                reply=attached.receive(time.monotonic()+5)
                                replies[reply["id"]]=reply
                            assert not replies[101]["result"].get("isError"),replies
                            assert replies[100]["result"].get("isError"),replies
                        else:
                            attached.tool("browser_record_start",args)
                            attached.tool("control_close",{})
                        def files():
                            return {str(p):p.read_bytes() for p in Path(root).rglob("*") if p.is_file() and "leases" not in p.parts}
                        before=files()
                        page=fixture.observe(owner,"Fixture login")
                        target=next(n["target"] for n in page["nodes"] if n["name"]=="Sign in")
                        owner.tool("control_act",{"target":target,"action":{"type":"click"}})
                        time.sleep(.8)
                        assert files()==before,"recording changed after close"
                        assert any(n["name"]=="After close" for n in fixture.observe(owner,"Fixture login")["nodes"])
                    finally:
                        attached.close()
                owned=fixture.Client(env)
                try:
                    launched,_=owned.tool("browser_open",{"browser":os.environ.get("AAD_TEST_BROWSER","edge"),
                        "headless":True,"url":f"http://127.0.0.1:{server.server_port}/login","record":{}})
                    recording=Path(launched["recording"]["out"])
                    owned.tool("control_close",{})
                    snapshot={str(p):p.read_bytes() for p in recording.rglob("*") if p.is_file()}
                    time.sleep(.5)
                    assert snapshot=={str(p):p.read_bytes() for p in recording.rglob("*") if p.is_file()}
                    assert json.loads((recording/"manifest.json").read_text())["status"]!="recording"
                    status,_=owned.tool("browser_status",{})
                    assert status["port"] is None,status
                finally:
                    owned.close()
            finally:
                owner.close()
    finally:
        server.shutdown()
        server.server_close()
    print("PASS: external recording close and startup cancellation; browser survives and files stop")

if __name__=="__main__":
    main()
