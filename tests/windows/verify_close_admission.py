"""Close's read-order boundary rejects later input before close worker runs."""
import json
import os
from pathlib import Path
import tempfile
import threading
import time
from http.server import ThreadingHTTPServer
from verify_mcp_browser_session import Client, Site, observe

with tempfile.TemporaryDirectory() as root:
    site = ThreadingHTTPServer(("127.0.0.1",0),Site)
    threading.Thread(target=site.serve_forever,daemon=True).start()
    env = dict(os.environ,AAD_RECORDINGS_DIR=root)
    owner = Client(env)
    attached = None
    gate = Path(root,"release")
    gate.touch()
    try:
        opened,_ = owner.tool("browser_open",{"browser":"edge","headless":True,
            "url":f"http://127.0.0.1:{site.server_port}/login"})
        attached = Client(dict(env,AAD_TEST_REQUEST_WORKER_GATE=str(gate)),opened["port"])
        page = observe(attached,"Fixture login")
        target = next(n["target"] for n in page["nodes"] if n["name"]=="Sign in")
        gate.unlink()
        for identifier,name,args in [(900,"control_close",{}),
                (901,"control_act",{"target":target,"action":{"type":"click"}})]:
            attached.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":identifier,
                "method":"tools/call","params":{"name":name,"arguments":args}})+"\n")
        attached.process.stdin.flush()
        reply = attached.receive(time.monotonic()+2)
        assert reply["id"]==901 and "session closing" in json.dumps(reply), reply
        # Close worker is still blocked: the old browser remains on login.
        assert any(n["name"]=="Sign in" for n in observe(owner,"Fixture login")["nodes"])
        gate.touch()
        close = attached.receive(time.monotonic()+5)
        assert close["id"]==900 and not close["result"].get("isError"),close
    finally:
        gate.touch()
        if attached:
            attached.close()
        owner.close()
        site.shutdown()
        site.server_close()
print("PASS: close read boundary rejects valid later click before worker release")
