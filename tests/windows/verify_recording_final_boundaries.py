"""Synthetic loopback CDP boundary regression; lifecycle-test-hooks required."""
import base64, hashlib, http.server, json, os, queue, socket, struct, subprocess, tempfile, threading, time
from pathlib import Path

AAD=os.environ.get('AAD_TEST_BINARY', str(Path(__file__).resolve().parents[2] / 'target/debug/aad'))

def main(mode, cli=False):
    ready=threading.Event(); release=threading.Event(); body_ids=[]
    class CDP(http.server.BaseHTTPRequestHandler):
        protocol_version='HTTP/1.1'
        def log_message(self,*args): pass
        def do_GET(self):
            if self.path=='/json/list':
                data=json.dumps([{'id':'fixture','type':'page','title':'fixture','url':'http://fixture.invalid','webSocketDebuggerUrl':f'ws://127.0.0.1:{self.server.server_port}/devtools/page/fixture'}]).encode()
                self.send_response(200); self.send_header('Content-Length',str(len(data))); self.end_headers(); self.wfile.write(data); return
            key=self.headers['Sec-WebSocket-Key']
            self.send_response(101); self.send_header('Upgrade','websocket'); self.send_header('Connection','Upgrade'); self.send_header('Sec-WebSocket-Accept',base64.b64encode(hashlib.sha1((key+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode()); self.end_headers()
            def send(value):
                data=json.dumps(value).encode(); head=bytes([129,len(data)]) if len(data)<126 else bytes([129,126])+struct.pack('!H',len(data)); self.wfile.write(head+data); self.wfile.flush()
            def event(method,params): send({'method':method,'params':params})
            def exact(n):
                data=self.rfile.read(n)
                if len(data)!=n: raise EOFError()
                return data
            snapshots=0
            try:
                while True:
                    head=exact(2); size=head[1]&127
                    if size==126: size=int.from_bytes(exact(2),'big')
                    elif size==127: size=int.from_bytes(exact(8),'big')
                    mask=exact(4) if head[1]&128 else None; raw=exact(size)
                    if mask: raw=bytes(v^mask[i%4] for i,v in enumerate(raw))
                    if head[0]&15==8: return
                    call=json.loads(raw); method=call['method']; result={}
                    if method=='Page.addScriptToEvaluateOnNewDocument': result={'identifier':'script'}
                    elif method=='Page.getFrameTree': result={'frameTree':{'frame':{'id':'frame'}}}
                    elif method=='Page.createIsolatedWorld': result={'executionContextId':1}
                    elif method=='Runtime.evaluate' and 'snapshot()' in call['params'].get('expression',''):
                        snapshots+=1
                        if snapshots==1:
                            for rid in (['one','two'] if mode=='dual' else ['one']):
                                event('Network.requestWillBeSent',{'requestId':rid,'frameId':'frame','type':'Fetch','request':{'method':'GET','url':f'http://fixture.invalid/api/{rid}'}})
                                event('Network.responseReceived',{'requestId':rid,'response':{'status':200,'mimeType':'application/json','headers':{}}})
                            ready.set(); assert release.wait(6),'initial snapshot not released'
                        else:
                            event('Network.loadingFinished',{'requestId':'one'})
                        result={'result':{'value':{'url':'http://fixture.invalid','title':'fixture','nodes':[],'document_id':'fixture'}}}
                    elif method=='Network.getResponseBody':
                        rid=call['params']['requestId']; body_ids.append(rid)
                        if mode=='dual':
                            if rid=='one': event('Network.loadingFinished',{'requestId':'two'})
                            result={'body':json.dumps({'id':rid}),'base64Encoded':False}
                        else:
                            if mode=='close': self.connection.sendall(b'\x88\x02\x03\xe8')
                            self.connection.shutdown(socket.SHUT_RDWR); return
                    send({'id':call['id'],'result':result})
            except (EOFError,OSError): pass
    server=http.server.ThreadingHTTPServer(('127.0.0.1',0),CDP); server.daemon_threads=True
    threading.Thread(target=server.serve_forever,daemon=True).start()
    with tempfile.TemporaryDirectory(prefix='aad-current-t47-') as root:
        out=Path(root)/'session'; gate=Path(root)/'body'; responses=queue.Queue()
        env=dict(os.environ,AAD_RECORDINGS_DIR=root,AAD_TEST_BODY_GATE=str(gate))
        args=[AAD,'browser-record','start','--cdp-port',str(server.server_port),'--page','fixture','--out',str(out),'--no-screenshots','--no-follow-popups'] if cli else [AAD,'mcp','--allow-input','--browser-port',str(server.server_port)]
        proc=subprocess.Popen(args,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,env=env)
        def read():
            if cli:
                responses.put(json.loads(proc.stdout.read()))
                return
            for line in proc.stdout: responses.put(json.loads(line))
        threading.Thread(target=read,daemon=True).start()
        def rpc(rid,name,args):
            proc.stdin.write(json.dumps({'jsonrpc':'2.0','id':rid,'method':'tools/call','params':{'name':name,'arguments':args}})+'\n'); proc.stdin.flush()
        try:
            if not cli:
                rpc(1,'browser_record_start',{'page_id':'fixture','record':{'body_limit':4096,'screenshots':False,'follow_popups':False}})
                started=responses.get(timeout=6); out=Path(json.loads(started['result']['content'][0]['text'])['out'])
            assert ready.wait(6),'no initial snapshot'
            before=time.monotonic()
            if cli:
                request=subprocess.run([AAD,'browser-record','stop','--out',str(out)],capture_output=True,text=True,timeout=4); assert request.returncode==0,request.stderr
            else:
                rpc(2,'browser_record_stop',{})
                limit=time.monotonic()+3
                while not Path(str(gate)+'.stop').exists():
                    assert time.monotonic()<limit,'stop did not enter'; threading.Event().wait(.005)
            release.set()
            if cli:
                proc.wait(timeout=6); stopped=responses.get(timeout=2)
            else:
                stopped=json.loads(responses.get(timeout=6)['result']['content'][0]['text'])['summary']
            manifest=json.loads((out/'manifest.json').read_text()); records=[json.loads(x) for x in (out/'evidence/network.jsonl').read_text().splitlines()]
            result={'mode':mode,'cli':cli,'elapsed':round(time.monotonic()-before,3),'body_commands':body_ids,'summary':stopped,'manifest_status':manifest['status'],'manifest_error':manifest['error'],'network':[{'request_id':r['request_id'],'body':r.get('body'),'body_error':r.get('body_error')} for r in records]}
            if cli: result['exit_code']=proc.returncode
            print(json.dumps(result),flush=True)
            if mode=='dual':
                assert body_ids==['one','two'],result
                assert {r['request_id']:json.loads(r['body'])['id'] for r in records}=={'one':'one','two':'two'},result
                assert manifest['status']=='complete' and stopped['recorded'] is True,result
            else:
                assert manifest['status']=='incomplete' and 'disconnected' in manifest['error'],result
                assert len(records)==1 and records[0]['body_error'],result
                if cli: assert proc.returncode!=0,'CLI final-body disconnect exits successfully'
                else: assert stopped['status']=='incomplete' and stopped['recorded'] is False,result
        finally:
            release.set()
            if proc.poll() is None:
                proc.stdin.close()
                try: proc.wait(timeout=4)
                except subprocess.TimeoutExpired: proc.kill(); proc.wait()
            server.shutdown(); server.server_close()

if __name__=='__main__':
    import sys
    main(sys.argv[1],len(sys.argv)>2)
