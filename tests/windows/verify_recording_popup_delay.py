"""Slow normal popup attach keeps its 10s budget; cancellation stays prompt."""
import http.server
import json
import os
from pathlib import Path
import tempfile
import threading
import time
import subprocess
import verify_mcp_browser_session as fixture
from verify_recording_stop import wait

def main():
    fixture.LOGIN=fixture.LOGIN.replace('''<button id="go" onclick="location.href='/home'">Sign in</button>''', '''<a href="/home" target="_blank" rel="opener">Sign in</a>''')
    server=http.server.ThreadingHTTPServer(("127.0.0.1",0),fixture.Site)
    threading.Thread(target=server.serve_forever,daemon=True).start()
    try:
        for cancel in (False,True):
            with tempfile.TemporaryDirectory(prefix="aad-popup-delay-") as root:
                gate=Path(root)/"popup"
                client=fixture.Client(dict(os.environ,AAD_RECORDINGS_DIR=root,AAD_TEST_POPUP_ATTACH_GATE=str(gate),AAD_CONTROL_LEASE_DIR=root+"/leases"))
                try:
                    opened,_=client.tool("browser_open",{"browser":"edge","headless":True,"url":f"http://127.0.0.1:{server.server_port}/login","record":{"body_limit":4096}})
                    subprocess.run(["node","--input-type=module","-e", """
const targets=await (await fetch(`http://127.0.0.1:${process.argv[1]}/json/list`)).json();
const page=targets.find(t=>t.id===process.argv[2]);
const ws=new WebSocket(page.webSocketDebuggerUrl);
await new Promise((resolve,reject)=>{ws.onopen=resolve;ws.onerror=reject;});
const response=new Promise(resolve=>ws.onmessage=e=>{const r=JSON.parse(e.data);if(r.id===1)resolve(r);});
ws.send(JSON.stringify({id:1,method:'Runtime.evaluate',params:{expression:"window.open('/home')",userGesture:true}}));
const result=await response;ws.close();if(result.error)throw Error(JSON.stringify(result));
""",str(opened["port"]),opened["page_id"]],check=True,timeout=5)
                    wait(gate)
                    if cancel:
                        started=time.monotonic()
                        closed,_=client.tool("browser_close",{})
                        assert time.monotonic()-started<1, closed
                        summary=closed["recording"]["summary"]
                        assert summary["status"]=="incomplete",summary
                    else:
                        # Deliberate 2.5s latency within the actual attach deadline.
                        time.sleep(2.5)
                        Path(str(gate)+".release").touch()
                        wait(Path(str(gate)+".attached"))
                        stopped,_=client.tool("browser_record_stop",{})
                        assert stopped["summary"]["recorded"] is True,stopped
                    manifest=json.loads((Path(opened["recording"]["out"])/"manifest.json").read_text())
                    assert manifest["status"]==("incomplete" if cancel else "complete"),manifest
                    print("PASS popup", "cancelled" if cancel else "2.5s attach")
                finally:
                    client.close()
    finally:
        server.shutdown()

if __name__=="__main__": main()
