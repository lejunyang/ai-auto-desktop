"""Own-window scale verification; invoked by verify-native-scale.ps1."""
import argparse, base64, json, os, pathlib, selectors, subprocess, time, uuid

parser = argparse.ArgumentParser()
for name in ('aad', 'helper', 'capture-helper', 'fixture', 'output'):
    parser.add_argument('--' + name, required=True)
args = parser.parse_args()
out = pathlib.Path(args.output)
out.mkdir(parents=True, exist_ok=True)
env = os.environ | {'AAD_LINUX_XTEST_HELPER': args.helper, 'AAD_LINUX_CAPTURE_HELPER': args.capture_helper}

class Client:
    def __init__(self, label):
        self.sid = None
        self.p = subprocess.Popen([args.aad, 'control', 'serve', '--allow-input'], env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(out/(label+'.stderr'), 'w'), text=True)
        self.log = open(out/(label+'.ndjson'), 'w')
        self.data('capabilities')
    def call(self, method, **kw):
        req = {'request_id':str(uuid.uuid4()), 'session_id':self.sid, 'timeout_ms':12000,
            'operation':{'method':method, **kw}}
        self.p.stdin.write(json.dumps(req)+'\n'); self.p.stdin.flush()
        with selectors.DefaultSelector() as selector:
            selector.register(self.p.stdout,selectors.EVENT_READ)
            if not selector.select(20): raise RuntimeError('control timed out')
        result = json.loads(self.p.stdout.readline())
        self.sid = result['session_id']
        if method != 'targets':
            self.log.write(json.dumps({'request':req,'response':result},ensure_ascii=False)+'\n'); self.log.flush()
        return result
    def data(self, method, **kw):
        result = self.call(method, **kw)
        assert not result.get('error'), result
        return result['data']
    def close(self):
        try:
            if self.p.poll() is None: self.call('close')
        finally:
            self.p.stdin.close()
            try: self.p.wait(timeout=10)
            except subprocess.TimeoutExpired: self.p.kill(); self.p.wait()
            self.log.close()

rows = []
passed = False
try:
    for label, factor, ambiguous, qualified in [('one','1',False,True),('fractional','1.5',False,True),('two','2',False,True),('ambiguous','1.5',True,True),('unqualified','1.5',False,False)]:
        fixture_env = env | {'QT_SCALE_FACTOR':factor,'QT_ACCESSIBILITY':'1','QT_LINUX_ACCESSIBILITY_ALWAYS_ON':'1'}
        p = subprocess.Popen([args.fixture, *(['--ambiguous'] if ambiguous else [])],env=fixture_env,
            stdout=subprocess.PIPE,stderr=open(out/(label+'-fixture.stderr'),'w'),text=True)
        c = None
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(p.stdout,selectors.EVENT_READ)
                assert selector.select(10), 'fixture did not start'
            metrics = json.loads(p.stdout.readline())
            old_type = env.get('XDG_SESSION_TYPE')
            if not qualified: env['XDG_SESSION_TYPE'] = 'wayland'
            try: c = Client(label)
            finally:
                if old_type is None: env.pop('XDG_SESSION_TYPE',None)
                else: env['XDG_SESSION_TYPE'] = old_type
            for _ in range(30):
                owned = [r for r in c.data('targets')['resources'] if r.get('metadata',{}).get('process_id') == p.pid]
                if len(owned) == 1: break
                time.sleep(.1)
            assert len(owned) == 1
            rid = owned[0]['resource_id']
            def observe():
                result = c.data('observe',resource_id=rid,max_nodes=100,view={'mode':'nodes'})
                assert not result['truncated']
                return result
            def fresh(name): return next(n for n in observe()['nodes'] if n['name'] == name)
            results = []
            if ambiguous or not qualified:
                button = fresh('Scale click')
                assert button['provenance']['coordinate_space'] == 'unproven_toolkit'
                assert 'click' not in button['capabilities']
                refused = c.call('act',target=button['target'],action={'type':'click'})
                assert refused.get('error') and refused['effect'] == 'not_applied', refused
                shot = c.call('capture',target=fresh('Scale click')['target'])
                assert shot.get('error'), 'unproven coordinates allowed a screenshot'
                # Native semantic invocation remains usable without guessing pixels.
                c.data('act',target=fresh('Scale click')['target'],action={'type':'invoke'})
                for _ in range(20):
                    current = observe()
                    if any(n['name'] == 'Scale clicks 1' for n in current['nodes']): break
                    time.sleep(.05)
                assert any(n['name'] == 'Scale clicks 1' for n in current['nodes'])
                results += [{'refused':refused['error']['code'],'capture_refused':shot['error']['code'],'semantic_effect':True}]
                if ambiguous:
                    # Narrowing to one of the ambiguous frames must not make
                    # its private native-window binding unique.
                    observation = observe()
                    nodes = {n['node_id']:n for n in observation['nodes']}
                    frame = next(n for n in observation['nodes'] if n['name'] == 'Scale click')
                    while frame['parent_id'] != 'n0': frame = nodes[frame['parent_id']]
                    scoped = c.data('observe',resource_id=rid,max_nodes=100,view={'mode':'nodes','scope_target':frame['target']})
                    scoped_button = next(n for n in scoped['nodes'] if n['name'] == 'Scale click')
                    assert scoped_button['provenance']['coordinate_space'] == 'unproven_toolkit'
                    shot = c.call('capture',target=scoped_button['target'])
                    assert shot.get('error'), 'scope narrowed away an ambiguous window identity'
            else:
                for name, action in [('Scale click',{'type':'click'}),('Scale input',{'type':'key','keys':['f2']})]:
                    result = c.data('act',target=fresh(name)['target'],action=action)
                    results.append({'name':name,'result':result})
                current = observe()
                assert any(n['name'] == 'Scale clicks 1' for n in current['nodes'])
                assert any(n['name'] == 'Scale input' and n.get('value') == 'F2 received' for n in current['nodes'])
                button = fresh('Scale click')
                shot = c.data('capture',target=button['target'])
                (out/(label+'.png')).write_bytes(base64.b64decode(shot['data']))
                assert (shot['width'],shot['height']) == (button['bounds']['width'],button['bounds']['height'])
            rows.append({'case':label,'metrics':metrics,'results':results,'passed':True})
            print('PASS: '+label,flush=True)
        finally:
            try:
                if c: c.close()
            finally:
                if p.poll() is None: p.terminate()
                try: p.wait(timeout=8)
                except subprocess.TimeoutExpired: p.kill(); p.wait()
    passed = True
finally:
    (out/'summary.json').write_text(json.dumps({'passed':passed,'checks':rows},ensure_ascii=False,indent=2))
