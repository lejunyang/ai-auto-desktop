"""Real generated QR images; owner-only delivery mocked, no external services."""
import http.server
import base64
import subprocess
import json
import os
from pathlib import Path
import tempfile
import threading
import time
from verify_mcp_browser_session import Client, observe, REPO, AAD

phase = "one"
class Site(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/phase"):
            body = json.dumps(phase).encode()
            mime = "application/json"
        elif self.path.startswith("/qr"):
            body = (Path(REPO)/"crates/aad-control/target/qr-fixtures"/("two.png" if phase=="two" else "one.png")).read_bytes()
            mime = "image/png"
        elif self.path.startswith("/carrier"):
            kind=self.path.split("?")[-1]
            carriers={
                "canvas": '<canvas role="img" aria-label="Login QR" width="256" height="256"></canvas><script>let i=new Image;i.onload=()=>document.querySelector("canvas").getContext("2d").drawImage(i,0,0,256,256);i.src="/qr"</script>',
                "svg": '<svg role="img" aria-label="Login QR" width="256" height="256"><image href="/qr" width="256" height="256"/></svg>',
                "background": '<div role="img" aria-label="Login QR" style="width:256px;height:256px;background:white url(/qr) center/contain no-repeat"></div>',
                "iframe": '<iframe title="QR frame" width="300" height="300" src="/carrier?img"></iframe>',
                "img": '<img alt="Login QR" src="/qr">',
            }
            body=('<title>Fixture login</title>'+carriers[kind]).encode()
            mime="text/html"
        elif self.path.startswith("/large"):
            body = b'<title>Fixture login</title><canvas aria-label="Login QR" role="img" width="4000" height="4000"></canvas><script>let c=document.querySelector("canvas"),x=c.getContext("2d");x.fillStyle="white";x.fillRect(0,0,4000,4000);let i=new Image;i.onload=()=>x.drawImage(i,50,50);i.src="/qr"</script>'
            mime="text/html"
        else:
            body = b'''<title>Fixture login</title><img alt="Login QR" src="/qr"><p role="status"></p>
            <script>let old='one';setInterval(async()=>{let p=await(await fetch('/phase')).json();
            if(p!==old){old=p;document.querySelector('img').src='/qr?v='+p;
            document.querySelector('p').textContent=p==='expired'?'Expired':p==='confirmed'?'Signed in':''}},100)</script>'''
            mime = "text/html"
        self.send_response(200)
        self.send_header("Content-Type",mime)
        self.send_header("Content-Length",str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self,*args): pass

def main():
    global phase
    server=http.server.ThreadingHTTPServer(("127.0.0.1",0),Site)
    threading.Thread(target=server.serve_forever,daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="aad-qr-") as root:
        client=Client(dict(os.environ,AAD_RECORDINGS_DIR=root))
        delivered=[]
        def send_owner(event, images):
            # Host integration contract: only owner-private delivery, never payload text.
            assert "fixture-only-ticket" not in json.dumps(event)
            if images: delivered.append(("owner-private",event["version"],images[0]["data"]))
        try:
            client.tool("browser_open",{"browser":os.environ.get("AAD_TEST_BROWSER","edge"),"headless":True,
                "url":f"http://127.0.0.1:{server.server_address[1]}"})
            resource=observe(client,"Fixture login")["resource_id"]
            args={"resource_id":resource,"locator":{"name":"Login QR"},
                "expired":{"role":"status","name":"Expired"},"confirmed":{"role":"status","name":"Signed in"}}
            for state in ["one","two","expired","refreshed","confirmed"]:
                phase=state
                time.sleep(.3)
                started=time.monotonic()
                event,images=client.tool("control_watch_qr",args)
                event=event["data"]
                assert time.monotonic()-started<5,event
                assert event["event"]==({"one":"changed","two":"changed","refreshed":"changed"}.get(state,state)),(state,event)
                assert event["stop"]==(state=="confirmed"),event
                send_owner(event,images)
                if state=="refreshed":
                    assert args["previous"]=="expired" and event["version"]!="expired",event
                args["previous"]=event["version"]
                if state=="one":
                    unchanged, no_images=client.tool("control_watch_qr",args)
                    assert unchanged["data"]["event"]=="unchanged" and not no_images,unchanged
                    def send(identifier,name,arguments):
                        client.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":identifier,
                            "method":"tools/call","params":{"name":name,"arguments":arguments}})+"\n")
                        client.process.stdin.flush()
                    send(100,"control_watch_qr",dict(args,request_id="qr-cancellable"))
                    time.sleep(.3)
                    send(101,"control_cancel",{"target_request_id":"qr-cancellable"})
                    answers={}
                    deadline=time.monotonic()+3
                    while len(answers)<2:
                        reply=client.receive(deadline)
                        answers[reply["id"]]=reply
                    assert "CANCELLED" in json.dumps(answers[100]),answers
                    assert "true" in json.dumps(answers[101]),answers
            assert len(delivered)==3 and delivered[0][1]!=delivered[1][1],delivered
            phase="one"
            for carrier in ["canvas","svg","background","iframe"]:
                probe=Client(dict(os.environ,AAD_RECORDINGS_DIR=root))
                try:
                    probe.tool("browser_open",{"browser":os.environ.get("AAD_TEST_BROWSER","edge"),
                        "headless":True,"url":f"http://127.0.0.1:{server.server_address[1]}/carrier?{carrier}"})
                    rid=observe(probe,"Fixture login")["resource_id"]
                    time.sleep(.3)
                    event,images=probe.tool("control_watch_qr",{"resource_id":rid,"locator":{"name":"QR frame" if carrier=="iframe" else "Login QR"}})
                    assert event["data"]["event"]=="changed" and not event["data"]["stop"],(carrier,event)
                    assert len(images)==1,(carrier,event)
                    checked=subprocess.run([AAD,"--aad-qr-decode"],input=base64.b64decode(images[0]["data"]),
                        capture_output=True,timeout=10,check=True)
                    decoded=json.loads(checked.stdout)
                    assert decoded["result"]["status"]=="identified",(carrier,decoded)
                finally:
                    probe.close()
            # A held decoder must terminate without publishing its candidate.
            for cancel in [False, True]:
                marker=Path(root)/("cancel-decoder" if cancel else "timeout-decoder")
                probe=Client(dict(os.environ,AAD_RECORDINGS_DIR=root,AAD_TEST_QR_BARRIER=str(marker)))
                try:
                    probe.tool("browser_open",{"browser":os.environ.get("AAD_TEST_BROWSER","edge"),
                        "headless":True,"url":f"http://127.0.0.1:{server.server_address[1]}/large"})
                    rid=observe(probe,"Fixture login")["resource_id"]
                    probe.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":200,"method":"tools/call",
                        "params":{"name":"control_watch_qr","arguments":{"resource_id":rid,"locator":{"name":"Login QR"},
                        "request_id":"held-qr","timeout_ms":10000 if cancel else 1500}}})+"\n")
                    probe.process.stdin.flush()
                    started=time.monotonic()
                    if cancel:
                        while not marker.exists():
                            assert time.monotonic()-started<5,"decoder not reached"
                            time.sleep(.01)
                        probe.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":201,"method":"tools/call",
                            "params":{"name":"control_cancel","arguments":{"target_request_id":"held-qr"}}})+"\n")
                        probe.process.stdin.flush()
                    replies={}
                    while 200 not in replies or (cancel and 201 not in replies):
                        reply=probe.receive(time.monotonic()+3)
                        replies[reply["id"]]=reply
                    assert ("CONTROL.CANCELLED" if cancel else "CONTROL.TIMEOUT") in json.dumps(replies[200]),replies
                    assert not any(c["type"]=="image" for c in replies[200]["result"]["content"]),replies
                    if cancel: assert "cancellation_requested" in json.dumps(replies[201]),replies
                    assert time.monotonic()-started<6,replies
                finally:
                    probe.close()
        finally:
            client.close()
            server.shutdown()
            server.server_close()
    print("PASS: true QR images, changed/expired/confirmed events, owner-private mock delivery")

if __name__=="__main__": main()
