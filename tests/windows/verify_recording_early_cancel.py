"""Synthetic loopback CDP boundary regression; lifecycle-test-hooks required."""
import json, os, queue, subprocess, tempfile, threading, time
from pathlib import Path
with tempfile.TemporaryDirectory(prefix='aad-early-current-') as root:
    gate=Path(root)/'release'; cancelled=Path(root)/'cancelled'
    env=dict(os.environ,AAD_TEST_REQUEST_WORKER_GATE=str(gate),AAD_TEST_CANCELLED=str(cancelled))
    proc=subprocess.Popen([os.environ.get('AAD_TEST_BINARY', str(Path(__file__).resolve().parents[2] / 'target/debug/aad')),'mcp','--allow-input'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,env=env)
    replies=queue.Queue()
    def read():
        for line in proc.stdout: replies.put(json.loads(line))
    threading.Thread(target=read,daemon=True).start()
    def send(value): proc.stdin.write(json.dumps(value)+'\n'); proc.stdin.flush()
    stop={'jsonrpc':'2.0','id':900,'method':'tools/call','params':{'name':'browser_record_stop','arguments':{}}}
    try:
        send(stop); send({'jsonrpc':'2.0','method':'notifications/cancelled','params':{'requestId':900}})
        deadline=time.monotonic()+3
        while not cancelled.exists():
            assert time.monotonic()<deadline,'cancel not registered'; threading.Event().wait(.005)
        gate.touch(); first=replies.get(timeout=3)
        assert first['error']['code']==-32800,first
        send(stop); second=replies.get(timeout=3)
        payload=json.loads(second['result']['content'][0]['text'])
        assert payload['code']=='BROWSER.NOT_RECORDING',second
        print(json.dumps({'cancel_before_worker':first,'same_id_next_generation':payload}))
    finally:
        gate.touch(); proc.stdin.close()
        try: proc.wait(timeout=3)
        except subprocess.TimeoutExpired: proc.kill(); proc.wait()
