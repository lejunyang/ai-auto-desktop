"""Stop then close/EOF while an actually sent CDP command awaits its reply."""
import http.server
import json
import os
from pathlib import Path
import tempfile
import threading
import time
import verify_mcp_browser_session as fixture
from verify_recording_stop import wait
from cdp_body_proxy import Proxy

def main():
    server=http.server.ThreadingHTTPServer(('127.0.0.1',0),fixture.Site)
    threading.Thread(target=server.serve_forever,daemon=True).start()
    try:
        for eof in (False,True):
            with tempfile.TemporaryDirectory(prefix='aad-inflight-') as root:
                marker=Path(root)/'waiting'
                env=dict(os.environ,AAD_RECORDINGS_DIR=root,AAD_CONTROL_LEASE_DIR=root+'/leases')
                owner=fixture.Client(env)
                client=proxy=None
                try:
                    opened,_=owner.tool('browser_open',{'browser':'edge','headless':True,'url':f'http://127.0.0.1:{server.server_port}/login'})
                    proxy=Proxy(opened['port'])
                    client=fixture.Client(dict(env,AAD_TEST_STOP_WAITING=str(marker)),proxy.server.server_port)
                    started,_=client.tool('browser_record_start',{'page_id':opened['page_id'],'record':{'body_limit':4096}})
                    # Trigger real network traffic only in the owned fixture browser.
                    page=fixture.observe(owner,'Fixture login')
                    button=fixture.node(page,lambda n:n['name']=='Sign in')
                    owner.tool('control_act',{'target':button['target'],'action':{'type':'click'}})
                    assert proxy.received.wait(5), 'proxy never received getResponseBody'
                    assert proxy.held.wait(5), 'proxy never received the browser body reply'
                    client.process.stdin.write(json.dumps({'jsonrpc':'2.0','id':900,'method':'tools/call','params':{'name':'browser_record_stop','arguments':{}}})+'\n')
                    client.process.stdin.flush()
                    wait(marker)
                    before=time.monotonic()
                    if eof:
                        client.process.stdin.close()
                        client.process.wait(timeout=1.5)
                    else:
                        client.process.stdin.write(json.dumps({'jsonrpc':'2.0','id':901,'method':'tools/call','params':{'name':'control_close','arguments':{}}})+'\n')
                        client.process.stdin.flush()
                        replies={}
                        while len(replies)<2:
                            answer=client.receive(before+1.5)
                            replies[answer['id']]=answer
                        stopped=json.loads(replies[900]['result']['content'][0]['text'])
                        assert stopped['summary']['status']=='incomplete',stopped
                        assert 'shutdown' in stopped['summary']['error'],stopped
                    elapsed=time.monotonic()-before
                    assert elapsed<1.5, elapsed
                    manifests=list(Path(root).rglob('manifest.json'))
                    assert len(manifests)==1, manifests
                    out=manifests[0].parent
                    manifest=json.loads(manifests[0].read_text())
                    assert manifest['status']=='incomplete' and 'shutdown' in manifest['error'],manifest
                    records=[json.loads(line) for line in (out/'evidence/network.jsonl').read_text().splitlines()]
                    assert any(r.get('body_status')=='unavailable' and 'cancelled' in r.get('body_error','') for r in records),records
                    snapshot={str(p):p.read_bytes() for p in out.rglob('*') if p.is_file()}
                    # Allow stale worker writes to expose themselves after the join boundary.
                    time.sleep(.15)
                    assert snapshot=={str(p):p.read_bytes() for p in out.rglob('*') if p.is_file()}
                    print(json.dumps({'eof':eof,'elapsed':elapsed,'actual_reply_withheld':True}))
                finally:
                    if client: client.close()
                    if proxy: proxy.close()
                    owner.close()
    finally:
        server.shutdown()

if __name__=='__main__': main()
