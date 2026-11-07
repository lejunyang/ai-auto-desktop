"""Real Chromium snapshot starvation regression; only loopback owned profiles."""
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import urllib.request
import verify_mcp_browser_session as fixture
from cdp_body_proxy import Proxy


def evaluate(port, page, expression):
    subprocess.run(['node', '--input-type=module', '-e', '''
const targets=await (await fetch(`http://127.0.0.1:${process.argv[1]}/json/list`)).json();
const ws=new WebSocket(targets.find(t=>t.id===process.argv[2]).webSocketDebuggerUrl);
await new Promise((ok,no)=>{ws.onopen=ok;ws.onerror=no;});
const reply=new Promise(ok=>ws.onmessage=e=>{const r=JSON.parse(e.data);if(r.id===1)ok(r);});
ws.send(JSON.stringify({id:1,method:'Runtime.evaluate',params:{expression:process.argv[3],userGesture:true}}));
const result=await reply;ws.close();if(result.error)throw Error(JSON.stringify(result));
''', str(port), page, expression], check=True, timeout=10)


def main():
    fixture.HOME = '<title>Sampling popup</title><button>POPUP_FAIRNESS_EVIDENCE</button>'
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), fixture.Site)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix='aad-sampling-') as root:
            env = dict(os.environ, AAD_RECORDINGS_DIR=root, AAD_CONTROL_LEASE_DIR=root+'/leases')
            owner = fixture.Client(env)
            client = proxy = None
            try:
                opened, _ = owner.tool('browser_open', {'browser':'edge','headless':True,
                    'url':f'http://127.0.0.1:{server.server_port}/login'})
                page = opened['page_id']
                def snapshot(message):
                    return message.get('method') == 'Runtime.evaluate' and message.get('params', {}).get('expression') == 'globalThis.__aadCdp ? globalThis.__aadCdp.snapshot() : null'
                contexts = {}
                churn = threading.Event()
                rebuilt = []
                def track_context(path, message):
                    if message.get('method') == 'Runtime.executionContextCreated':
                        c = message['params']['context']
                        if c.get('name','').startswith('aad-record-'):
                            contexts[(path, c['id'])] = c
                    return False
                def on_held(path, request):
                    key = (path, request['params']['contextId'])
                    if not churn.is_set() or key not in contexts:
                        return []
                    c = dict(contexts[key])
                    if rebuilt and c['id'] != rebuilt[0]:
                        return []
                    c['uniqueId'] = 'test-rebuilt-'+str(len(rebuilt))
                    rebuilt.append(c['id'])
                    return [{'method':'Runtime.executionContextDestroyed','params':{'executionContextId':c['id']}},
                            {'method':'Runtime.executionContextCreated','params':{'context':c}}]
                proxy = Proxy(opened['port'], lambda path, msg: path.endswith('/'+page) and snapshot(msg), track_context, on_held)
                evaluate(opened['port'], page, "window.open('/home')")
                with urllib.request.urlopen(f"http://127.0.0.1:{opened['port']}/json/list") as response:
                    order_popup = next(t['id'] for t in json.load(response) if t.get('title') == 'Sampling popup')
                evaluate(opened['port'], order_popup, "for(let i=0;i<2;i++){let f=document.createElement('iframe');f.srcdoc='<button>FRAME_'+i+'</button>';document.body.appendChild(f)}")
                subprocess.run(['cargo','test','-p','aad-browser','real_context_recreation_preserves_waiting_frame_order','--','--ignored'],
                               env=dict(env,AAD_SAMPLING_PORT=str(proxy.server.server_port),AAD_SAMPLING_PAGE=page,AAD_SAMPLING_POPUP=order_popup),check=True,timeout=60)
                assert proxy.held.is_set(), 'specialist recorder did not hit slow snapshot'
                evaluate(opened['port'], order_popup, 'window.close()')
                # A new recorder needs new barriers and counters. Late replies
                # from the specialist must never satisfy this phase's barrier.
                proxy.close()
                contexts.clear()
                release_initial = os.environ.get('AAD_TEST_RELEASE_INITIAL') == '1'
                proxy = Proxy(opened['port'], lambda path, msg: (not release_initial or churn.is_set()) and path.endswith('/'+page) and snapshot(msg), track_context, on_held)
                client = fixture.Client(env, proxy.server.server_port)
                client.tool('browser_record_start', {'page_id':page,'record':{'screenshots':False}})
                assert proxy.held.wait(5), 'no actual main-page snapshot reply withheld'
                evaluate(opened['port'], page, "window.open('/home')")
                deadline = time.monotonic()+12
                found = False
                while time.monotonic() < deadline:
                    with proxy.condition:
                        proxy.condition.wait(.1)
                        rounds = sum(path.endswith('/'+page) and snapshot(msg) for path,msg in proxy.commands)
                    evidence = list(Path(root).rglob('ui.jsonl'))
                    found = any('POPUP_FAIRNESS_EVIDENCE' in p.read_text() for p in evidence)
                    if found or rounds >= 8:
                        break
                assert found, f'popup starved after {rounds} main snapshot attempts'
                assert rounds <= 8, rounds
                def evidence_contains(*labels):
                    text = '\n'.join(p.read_text() for p in Path(root).rglob('ui.jsonl'))
                    return all(label in text for label in labels)
                def until(predicate, label):
                    end = time.monotonic()+12
                    while time.monotonic() < end:
                        if predicate(): return
                        with proxy.condition: proxy.condition.wait(.1)
                    raise AssertionError(label)
                def targets():
                    with urllib.request.urlopen(f"http://127.0.0.1:{opened['port']}/json/list") as response:
                        return json.load(response)
                popup = next(t['id'] for t in targets() if t.get('title') == 'Sampling popup')
                # Each page has two real child frames. The main page remains slow;
                # the popup must still produce both frame snapshots.
                for target in (page, popup):
                    evaluate(opened['port'], target, "for(let i=0;i<2;i++){let f=document.createElement('iframe');f.srcdoc='<button>FRAME_'+i+'_EVIDENCE</button>';document.body.appendChild(f)}")
                until(lambda: evidence_contains('FRAME_0_EVIDENCE','FRAME_1_EVIDENCE'), 'two-frame evidence missing')
                print(json.dumps({'case':'two_pages_two_frames','actual_frame_ui':True}))
                # Inject destroy/create between samples using the same numeric ID.
                # Snapshot results still come from Chromium; only lifecycle delivery
                # is controlled to deterministically reproduce ID reuse.
                def snapshot_gaps():
                    return [json.loads(line) for p in Path(root).rglob('health.jsonl') for line in p.read_text().splitlines()
                            if json.loads(line).get('kind') == 'ui_snapshot_unavailable']
                before_churn = len(snapshot_gaps())
                churn.set()
                for generation in range(3):
                    previous = len(rebuilt)
                    until(lambda: len(rebuilt) > previous, 'context recreation barrier missing')
                    label = f'CHURN_POPUP_{generation}'
                    evaluate(opened['port'], popup, f'document.querySelector("button").textContent={json.dumps(label)}')
                    until(lambda: evidence_contains(label), 'waiting popup starved by recreated context')
                churn.clear()
                assert len(set(rebuilt)) == 1, rebuilt
                until(lambda: len([r for r in snapshot_gaps()[before_churn:] if r['detail']['page'] == page and r['detail']['context'] == rebuilt[0]]) >= 3, 'recreated context generations missing gaps')
                matching = [r for r in snapshot_gaps()[before_churn:] if r['detail']['page'] == page and r['detail']['context'] == rebuilt[0]]
                assert len(matching) >= 3, matching
                assert len({r['detail']['identity'] for r in matching}) == len(matching), matching
                print(json.dumps({'case':'same_numeric_context_recreation','rebuilds':len(rebuilt),'popup_generations':3,'injected_lifecycle':True}))
                # Remove a sampled page, then add another with distinct evidence.
                evaluate(opened['port'], popup, 'window.close()')
                until(lambda: all(t['id'] != popup for t in targets()), 'popup did not close')
                fixture.HOME = '<title>Replacement popup</title><button>REPLACEMENT_EVIDENCE</button>'
                evaluate(opened['port'], page, "window.open('/home')")
                until(lambda: evidence_contains('REPLACEMENT_EVIDENCE'), 'replacement page starved')
                print(json.dumps({'case':'page_remove_add','actual_replacement_ui':True}))
                stopped, _ = client.tool('browser_record_stop', {})
                print(json.dumps({'case':'slow_main_popup','main_attempts':rounds,'actual_popup_ui':found,'summary':stopped}))
                client.close()
                proxy.close()
                for target in targets():
                    if target.get('type') == 'page' and target['id'] != page:
                        evaluate(opened['port'], target['id'], 'window.close()')
                cleared = threading.Event()
                suppress = threading.Event()
                def drop_event(path, message):
                    if path.endswith('/'+page) and suppress.is_set():
                        if message.get('method') == 'Runtime.executionContextsCleared':
                            cleared.set()
                        return message.get('method') == 'Runtime.executionContextCreated'
                    return False
                proxy = Proxy(opened['port'], lambda path,msg: False, drop_event)
                marker = Path(root)/'contexts-cleared.json'
                release = Path(root)/'contexts-release'
                client = fixture.Client(dict(env, AAD_TEST_CONTEXTS_CLEARED=str(marker), AAD_TEST_CONTEXTS_RELEASE=str(release)), proxy.server.server_port)
                started, _ = client.tool('browser_record_start', {'page_id':page,'record':{'screenshots':True}})
                out = Path(started['out'])
                until(lambda: bool(list(out.rglob('*.png'))), 'baseline screenshot missing')
                prior = {str(p) for p in out.rglob('*.png')}
                suppress.set()
                fixture.HOME = '<title>Contextless screenshot</title><body style="background:rgb(8,77,123)">ZERO_CONTEXT_CAPTURE</body>'
                evaluate(opened['port'], page, "location.href='/home?zero-context'")
                assert cleared.wait(5), 'real context clear event missing'
                until(marker.exists, 'recorder did not process context clear')
                assert json.loads(marker.read_text()) == {'page':page,'contexts':0}
                # The recorder is paused after clearing: exclude every PNG
                # produced before that point, including navigation races.
                prior = {str(p) for p in out.rglob('*.png')}
                release.touch()
                until(lambda: any(str(p) not in prior and p.read_bytes().startswith(b'\x89PNG\r\n\x1a\n') for p in out.rglob('*.png')), 'no actual PNG after context clearing')
                client.tool('browser_record_stop', {})
                steps = [json.loads(line) for line in (out/'steps.jsonl').read_text().splitlines()]
                assert any(s['page_id'] == page and any(str(out/shot['path']) not in prior for shot in s['screenshots']) for s in steps), steps
                print(json.dumps({'case':'zero_context_screenshot','actual_png':True,'creation_events_withheld':True}))
            finally:
                if client: client.close()
                if proxy: proxy.close()
                owner.close()
    finally:
        server.shutdown()


if __name__ == '__main__':
    main()
