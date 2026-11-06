# Invoked by verify-code-paste.ps1. Only a new isolated Code profile is touched.
# All input goes through aad; CDP is restricted to read-only verification.
import argparse, base64, hashlib, json, os, pathlib, selectors, socket
import subprocess, time, types, urllib.request, uuid

parser=argparse.ArgumentParser()
parser.add_argument('--aad', required=True)
parser.add_argument('--helper', required=True)
parser.add_argument('--code', required=True)
parser.add_argument('--node', required=True)
parser.add_argument('--output', required=True)
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
def diagnostic(label):
    expression="(()=>{let a=document.activeElement;return {focused:{tag:a?.tagName,role:a?.getAttribute('role'),label:a?.getAttribute('aria-label'),value:a?.value,start:a?.selectionStart,end:a?.selectionEnd,classes:a?.className,editContext:a?.editContext?{text:a.editContext.text,start:a.editContext.selectionStart,end:a.editContext.selectionEnd}:null,domSelection:window.getSelection()?.toString(),selectionRects:[...document.querySelectorAll('.selected-text')].map(x=>({w:x.getBoundingClientRect().width,h:x.getBoundingClientRect().height}))},lines:[...document.querySelectorAll('.view-lines')].map(x=>x.innerText),dialogs:[...document.querySelectorAll('[role=dialog]')].map(x=>({label:x.getAttribute('aria-label'),text:x.innerText.slice(0,250)}))};})()"
    value=cdp('Runtime.evaluate',{'expression':expression,'returnByValue':True})['result'].get('value')
    rec('independent_'+label,state=value)
    if label in ('paste','save'):
        assert expected.strip() in '\n'.join(value['lines']).replace('\u00a0',' '), 'displayed editor content differs from requested paste'
try:
    class Client(common.Client):
        def call(self,method,**kw):
            if method!='targets':return super().call(method,**kw)
            old=self.log
            try:
                with open(os.devnull,'w') as sink:
                    self.log=sink;return super().call(method,**kw)
            finally:self.log=old
    c=Client(label='native')
    for _ in range(35):
        found=[r for r in c.data('targets')['resources']
               if r.get('metadata',{}).get('process_id') == p.pid]
        if len(found)==1:break
        time.sleep(.3)
    assert len(found)==1,{'owned':len(found)}
    app=found[0];rid=app['resource_id'];(common.OUT/'owned-resource.json').write_text(json.dumps(app))
    with urllib.request.urlopen(f'http://127.0.0.1:{port}/json/list', timeout=5) as response:pages=json.load(response)
    page=next(t for t in pages if t['type']=='page' and 'workbench' in t['url'])
    ws=page['webSocketDebuggerUrl']
    with urllib.request.urlopen(f'http://127.0.0.1:{port}/json/version', timeout=5) as response:browser_ws=json.load(response)['webSocketDebuggerUrl']
    ready_until=time.monotonic()+40
    while True:
        response=c.call('observe',resource_id=rid,max_nodes=2000,view={'mode':'nodes'})
        if response.get('error'):
            rec('startup_observation_retry',code=response['error']['code'])
            assert time.monotonic()<ready_until,response
            time.sleep(.3)
            continue
        o=response['data']
        if any(n['role']=='entry' and n['name']==fixture.name for n in o['nodes']):break
        assert time.monotonic()<ready_until,'Code editor did not finish startup'
        time.sleep(.25)
    dialogs=[n for n in o['nodes'] if n['role']=='dialog' and n['name']=='Welcome to Visual Studio Code']
    if dialogs:
        assert len(dialogs)==1
        scope=c.observe(rid,mode='nodes',scope_target=dialogs[0]['target']);assert not scope['truncated']
        close=next(n for n in scope['nodes'] if n['role']=='push_button' and n['name']=='Close')
        closepath=close['provenance']['object_path'];scopeid=scope['scope']['id']
        for action in [{'type':'focus'},{'type':'key','keys':['enter']}]:
            now=c.observe(rid,mode='nodes',scope_id=scopeid);assert not now['truncated']
            close=next(n for n in now['nodes'] if n['provenance']['object_path']==closepath)
            rec('close_welcome',action=action,result=c.data('act',target=close['target'],action=action))
    for _ in range(25):
        response=c.call('observe',resource_id=rid,max_nodes=2000,view={'mode':'nodes'})
        if response.get('error'):
            time.sleep(.15);continue
        o=response['data']
        if not any(n['role']=='dialog' and n['name']=='Welcome to Visual Studio Code' for n in o['nodes']):break
        time.sleep(.15)
    diagnostic('after_welcome')
    assert not any(n['role']=='dialog' and n['name']=='Welcome to Visual Studio Code' for n in o['nodes'])
    editors=[n for n in o['nodes'] if n['role']=='entry' and n['name']==fixture.name];assert len(editors)==1
    scope=c.observe(rid,mode='nodes',scope_target=editors[0]['target']);assert not scope['truncated']
    scopeid=scope['scope']['id'];editorpath=editors[0]['provenance']['object_path']
    def fresh():
        now=c.observe(rid,mode='nodes',scope_id=scopeid);assert not now['truncated']
        return next(n for n in now['nodes'] if n['provenance']['object_path']==editorpath)
    expected='AAD Electron 原生编辑与剪贴板验收 2026-11-07\n'
    for label,action in [('focus',{'type':'focus'}),('select_all',{'type':'key','keys':['ctrl','a']}),('refocus_after_select',{'type':'focus'}),('paste',{'type':'paste','text':expected}),('save',{'type':'key','keys':['ctrl','s']})]:
        rec(label,result=c.data('act',target=fresh()['target'],action=action))
        time.sleep(.15);diagnostic(label)
    for _ in range(30):
        actual=fixture.read_text()
        if actual==expected:break
        time.sleep(.1)
    rec('disk_readback',matches=actual==expected,value=actual)
    rec('ax_readback',value=fresh().get('value'))
    import base64
    shot=cdp('Page.captureScreenshot',{'format':'png'})
    (common.OUT/'independent-editor.png').write_bytes(base64.b64decode(shot['data']))
    assert actual==expected
    passed=True
    print('PASS: native Code paste, displayed text, saved file and screenshot independently verified',flush=True)
finally:
    try:
        if c:c.close()
    finally:
        if p.poll() is None:p.terminate()
        try:p.wait(timeout=8)
        except subprocess.TimeoutExpired:p.kill();p.wait()
        (common.OUT/'summary.json').write_text(json.dumps({'passed':passed,'helper_sha256':hashlib.sha256(pathlib.Path(args.helper).read_bytes()).hexdigest(),'checks':rows},ensure_ascii=False,indent=2))
