# Invoked by verify-canonical-invoke.ps1. Only a new isolated Code profile is touched.
# All input goes through aad; CDP is restricted to read-only verification.
import argparse, base64, hashlib, json, os, pathlib, selectors, socket
import subprocess, time, types, urllib.request, uuid

parser=argparse.ArgumentParser()
parser.add_argument('--aad', required=True)
parser.add_argument('--helper', required=True)
parser.add_argument('--code', required=True)
parser.add_argument('--node', required=True)
parser.add_argument('--output', required=True)
parser.add_argument('--qt-fixture', required=True)
args=parser.parse_args()
common=types.SimpleNamespace(OUT=pathlib.Path(args.output).resolve())
common.OUT.mkdir(parents=True, exist_ok=True)
os.environ['AAD_LINUX_XTEST_HELPER']=str(pathlib.Path(args.helper).resolve())

class ControlClient:
    def __init__(self, label='native'):
        self.p=subprocess.Popen([args.aad,'control','serve','--allow-input'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=open(common.OUT/(label+'-stderr.log'),'w'),text=True)
        self.sid=None
        self.log=open(common.OUT/(label+'.ndjson'),'w')
        self.call('capabilities')
    def call(self,method,**kw):
        request={'request_id':str(uuid.uuid4()),'session_id':self.sid,'timeout_ms':12000,'operation':{'method':method,**kw}}
        self.p.stdin.write(json.dumps(request)+'\n');self.p.stdin.flush()
        with selectors.DefaultSelector() as selector:
            selector.register(self.p.stdout, selectors.EVENT_READ)
            if not selector.select(timeout=20): raise RuntimeError('control response timed out')
        line=self.p.stdout.readline()
        if not line: raise RuntimeError('control server stopped')
        response=json.loads(line);self.sid=response['session_id']
        if method in ('observe','act'):
            self.log.write(json.dumps({'request':request,'response':response},ensure_ascii=False)+'\n');self.log.flush()
        return response
    def data(self,method,**kw):
        response=self.call(method,**kw)
        if response.get('error'): raise RuntimeError(response['error']['code'])
        return response['data']
    def observe(self,rid,**view):
        return self.data('observe',resource_id=rid,max_nodes=2000,view=view or {'mode':'nodes'})
    def close(self):
        try:
            if self.p.poll() is None:self.call('close')
        finally:
            self.p.stdin.close()
            try:self.p.wait(timeout=15)
            except subprocess.TimeoutExpired:self.p.kill();self.p.wait()
            self.log.close()
common.Client=ControlClient
profile=common.OUT/('profile-'+uuid.uuid4().hex);settings=profile/'User/settings.json';settings.parent.mkdir(parents=True,exist_ok=True)
settings.write_text(json.dumps({'window.restoreWindows':'none','workbench.startupEditor':'none','editor.accessibilitySupport':'on','workbench.enableExperiments':False,'telemetry.telemetryLevel':'off','update.mode':'none'}))
fixture=common.OUT/'aad-electron-native-effect.txt';fixture.write_text('Initial isolated Electron verification\n')
sock=socket.socket();sock.bind(('127.0.0.1',0));port=sock.getsockname()[1];sock.close()
p=subprocess.Popen([args.code,'--user-data-dir',str(profile),'--extensions-dir',str(common.OUT/'extensions'),'--disable-extensions','--force-renderer-accessibility','--new-window',f'--remote-debugging-port={port}',str(fixture)],stdout=open(common.OUT/'code-stdout.log','w'),stderr=open(common.OUT/'code-stderr.log','w'))
c=None;rows=[];ws=None;sequence=0;browser_ws=None;passed=False
def rec(case,**values):
    row={'case':case,**values};rows.append(row);print(json.dumps(row,ensure_ascii=False),flush=True)
def cdp(method,params={}):
    global sequence
    sequence+=1
    started=time.monotonic()
    output=subprocess.run([args.node,str(pathlib.Path(__file__).with_name('code_native_read.mjs')),ws],input=json.dumps({'id':sequence,'method':method,'params':params}),text=True,capture_output=True,timeout=20)
    transport={'sequence':sequence,'method':method,'elapsed_ms':round((time.monotonic()-started)*1000),'exit_code':output.returncode,'stderr':output.stderr}
    (common.OUT/f'cdp-{sequence}.json').write_text(json.dumps(transport))
    if output.returncode:
        rec('cdp_failed',**transport)
        if browser_ws:
            probe=subprocess.run([args.node,str(pathlib.Path(__file__).with_name('code_native_read.mjs')),browser_ws],input=json.dumps({'id':9000,'method':'Browser.getVersion','params':{}}),text=True,capture_output=True,timeout=20)
            rec('browser_transport_probe',exit_code=probe.returncode,stderr=probe.stderr,responded='result' in probe.stdout)
        raise RuntimeError('read-only CDP request failed; transport evidence saved')
    r=json.loads(output.stdout)
    assert 'error' not in r,r
    return r['result']
def welcome_present():
    value=cdp('Runtime.evaluate',{'expression':"[...document.querySelectorAll('[role=dialog]')].some(x=>x.getAttribute('aria-label')==='Welcome to Visual Studio Code')",'returnByValue':True})
    return value['result']['value']

def canonical_names(node):
    import dbus
    address=dbus.SessionBus().get_object('org.a11y.Bus','/org/a11y/bus').GetAddress(dbus_interface='org.a11y.Bus')
    bus=dbus.bus.BusConnection(str(address))
    proxy=bus.get_object(node['provenance']['bus_name'],node['provenance']['object_path'])
    action=dbus.Interface(proxy,'org.a11y.atspi.Action')
    listed=action.GetActions(timeout=5)
    return [{'index':i,'canonical':str(action.GetName(dbus.Int32(i),timeout=5)),
             'localized':str(item[0])} for i,item in enumerate(listed)]

try:
    c=ControlClient(label='code-native')
    for _ in range(50):
        found=[r for r in c.data('targets')['resources'] if r.get('metadata',{}).get('process_id')==p.pid]
        if len(found)==1: break
        time.sleep(.2)
    assert len(found)==1, 'isolated Code resource unavailable'
    rid=found[0]['resource_id']
    ready_until=time.monotonic()+40
    while True:
        response=c.call('observe',resource_id=rid,max_nodes=2000,view={'mode':'nodes'})
        if not response.get('error'):
            o=response['data']
            dialogs=[n for n in o['nodes'] if n['role']=='dialog' and n['name']=='Welcome to Visual Studio Code']
            if len(dialogs)==1: break
        assert time.monotonic()<ready_until, 'fresh Code Welcome dialog unavailable'
        time.sleep(.25)
    with urllib.request.urlopen(f'http://127.0.0.1:{port}/json/list',timeout=5) as response: pages=json.load(response)
    ws=next(t['webSocketDebuggerUrl'] for t in pages if t['type']=='page' and 'workbench' in t['url'])
    assert welcome_present(), 'independent Welcome precondition missing'
    (common.OUT/'code-before.png').write_bytes(base64.b64decode(cdp('Page.captureScreenshot',{'format':'png'})['data']))
    scope=c.observe(rid,mode='nodes',scope_target=dialogs[0]['target'])
    assert not scope['truncated']
    close=next(n for n in scope['nodes'] if n['role']=='push_button' and n['name']=='Close')
    actual=canonical_names(close)
    rec('code_canonical_names',actions=actual,advertised=close['capabilities'],selected=close['provenance'].get('native_invoke_name'))
    assert 'invoke' in close['capabilities']
    assert close['provenance']['native_invoke_name']=='press'
    assert actual[0]['canonical']=='press' and actual[0]['localized']==''
    started=time.monotonic()
    result=c.data('act',target=close['target'],action={'type':'invoke'})
    for _ in range(20):
        if not welcome_present(): break
        time.sleep(.1)
    assert not welcome_present(), 'semantic invoke did not close Welcome'
    rec('code_semantic_invoke',elapsed_ms=round((time.monotonic()-started)*1000),result=result,independent_dialog_absent=True)
    (common.OUT/'code-after.png').write_bytes(base64.b64decode(cdp('Page.captureScreenshot',{'format':'png'})['data']))
    c.close(); c=None
    p.terminate(); p.wait(timeout=10)

    fixture_env=os.environ|{'QT_ACCESSIBILITY':'1','QT_LINUX_ACCESSIBILITY_ALWAYS_ON':'1','QT_SCALE_FACTOR':'1'}
    p=subprocess.Popen([args.qt_fixture],env=fixture_env,stdout=subprocess.PIPE,
        stderr=open(common.OUT/'qt-fixture.stderr','w'),text=True)
    with selectors.DefaultSelector() as selector:
        selector.register(p.stdout,selectors.EVENT_READ)
        assert selector.select(10), 'Qt fixture did not start'
    assert p.stdout.readline().strip()=='READY'
    c=ControlClient(label='qt-native')
    for _ in range(40):
        owned=[r for r in c.data('targets')['resources'] if r.get('metadata',{}).get('process_id')==p.pid]
        if len(owned)==1: break
        time.sleep(.1)
    assert len(owned)==1
    rid=owned[0]['resource_id']
    o=c.observe(rid); assert not o['truncated']
    button=next(n for n in o['nodes'] if n['name']=='Invoke Qt fixture button')
    assert any(n['name']=='Qt fixture status idle' for n in o['nodes'])
    actual=canonical_names(button)
    rec('qt_canonical_names',actions=actual,advertised=button['capabilities'],selected=button['provenance'].get('native_invoke_name'))
    assert button['provenance']['native_invoke_name']=='Press'
    result=c.data('act',target=button['target'],action={'type':'invoke'})
    for _ in range(20):
        observed=c.observe(rid)
        if any(n['name']=='Qt fixture status invoked' for n in observed['nodes']): break
        time.sleep(.1)
    assert any(n['name']=='Qt fixture status invoked' for n in observed['nodes']), 'Qt label did not change'
    rec('qt_press_effect',result=result,displayed_label='Qt fixture status invoked')
    passed=True
    print('PASS: canonical Code invoke and Qt Press changed the displayed UI',flush=True)
finally:
    try:
        if c:c.close()
    finally:
        if p.poll() is None:p.terminate()
        try:p.wait(timeout=8)
        except subprocess.TimeoutExpired:p.kill();p.wait()
        (common.OUT/'summary.json').write_text(json.dumps({'passed':passed,'helper_sha256':hashlib.sha256(pathlib.Path(args.helper).read_bytes()).hexdigest(),'checks':rows},ensure_ascii=False,indent=2))
