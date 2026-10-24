import {test} from 'node:test';
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {mkdtemp,readFile,readdir,rm,stat} from 'node:fs/promises';
import {join,resolve} from 'node:path';
import {tmpdir} from 'node:os';
import {setTimeout as delay} from 'node:timers/promises';
import {fixture} from './fixture.mjs';

const aad=process.env.AAD_TEST_CLI || resolve('target/debug/aad');
function command(args) {
  const child=spawn(aad,args,{stdio:['ignore','pipe','pipe']});let stdout='',stderr='';
  child.stdout.on('data',d=>stdout+=d);child.stderr.on('data',d=>stderr+=d);
  const done=new Promise((resolve,reject)=>{child.on('error',reject);child.on('exit',(code,signal)=>resolve({code,signal,stdout,stderr}));});
  return {child,done};
}
async function until(fn,description) {
  for(let i=0;i<200;i++){try{const result=await fn();if(result)return result;}catch{}await delay(50);}
  throw new Error(`Timed out: ${description}`);
}
const json=async path=>JSON.parse(await readFile(path,'utf8'));
const jsonl=async path=>(await readFile(path,'utf8')).trim().split('\n').filter(Boolean).map(JSON.parse);

const html=`<!doctype html><meta charset="utf-8"><title>Recording fixture</title>
<label for="account">Account</label><input id="account"><input id="other"><input id="password" type="password">
<button id="login" onclick="openQr()">扫码登录</button>
<dialog id="qr" open hidden><h2>使用手机扫码</h2><canvas id="code" width="120" height="120"></canvas><output id="status">waiting</output></dialog>
<button id="profile" hidden>个人菜单</button><a id="next" href="/done">Continue</a>
<iframe src="/frame"></iframe><script>
async function openQr(){await fetch('/auth/qr/create?token=query-secret',{method:'POST',headers:{'Content-Type':'application/json','Authorization':'Bearer auth-secret'},body:JSON.stringify({account:document.getElementById('account').value,password:'password-secret'})});document.getElementById('qr').hidden=false;let c=document.getElementById('code').getContext('2d');c.fillStyle='white';c.fillRect(0,0,120,120);c.fillStyle='black';for(let x=0;x<12;x++)for(let y=0;y<12;y++)if((x+y)%3===0)c.fillRect(x*10,y*10,10,10);}
async function pollStatus(){const data=await(await fetch('/auth/qr/status')).json();document.getElementById('status').textContent=data.status;if(data.status==='confirmed'){document.getElementById('qr').hidden=true;document.getElementById('profile').hidden=false;}}
</script>`;

async function runCase(redacted) {
  let polls=0;const serverRequests=[];
  const f=await fixture((req,res)=>{
    serverRequests.push({url:req.url,authorization:req.headers.authorization,cookie:req.headers.cookie});
    if(req.url.startsWith('/auth/')){
      res.setHeader('Content-Type','application/json');
      res.end(JSON.stringify({status:req.url.includes('/status')?(++polls>=4?'confirmed':'waiting'):'created',token:'response-secret',qr_id:'fixture-qr'}));
    }else if(req.url==='/frame'){
      res.setHeader('Content-Type','text/html; charset=utf-8');res.end('<button id="frame-button" onclick="this.disabled=true">Frame action</button>');
    }else{
      res.setHeader('Content-Type','text/html; charset=utf-8');res.setHeader('Set-Cookie','sid=cookie-secret; Path=/; SameSite=Lax');
      res.end(req.url==='/done'?'<title>Done</title><h1>Finished</h1>':html);
    }
  });
  const root=await mkdtemp(join(tmpdir(),`aad-recording-${redacted?'redacted':'raw'}-`));
  const out=join(root,'session');let recorder;
  try {
    const listing=await command(['browser-record','targets','--cdp-port',String(f.port)]).done;
    assert.equal(listing.code,0,listing.stderr);
    assert.ok(JSON.parse(listing.stdout).pages.some(p=>p.id===f.target.id));
    recorder=command(['browser-record','start','--cdp-port',String(f.port),'--page',f.target.id,'--out',out,'--seconds','25',...(redacted?['--redact-network']:[])]);
    await until(async()=> (await json(join(out,'manifest.json'))).status==='recording','recorder ready');
    await until(async()=> (await readdir(join(out,'images'))).length>0,'baseline screenshot');
    // Explicitly observe and revalidate fixture elements before synthesizing input.
    assert.deepEqual(await f.cdp.eval(`Array.from(document.querySelectorAll('#account,#other,#login')).map(e=>e.id)`),['account','other','login']);
    await f.cdp.eval(`(()=>{for(const [id,value] of [['account','alice'],['other','second']]){const el=document.getElementById(id);if(!el||el.tagName!=='INPUT')throw Error('stale fixture');el.value=value;el.dispatchEvent(new Event('input',{bubbles:true}));}const button=document.getElementById('login');if(button.textContent!=='扫码登录')throw Error('stale fixture');button.click();})()`);
    await until(()=>f.cdp.eval(`!document.getElementById('qr').hidden`),'QR dialog visible');
    await delay(1000);
    for(let i=0;i<4;i++){await f.cdp.eval('pollStatus()');await delay(100);}
    assert.equal(await f.cdp.eval(`!document.getElementById('profile').hidden`),true);
    await delay(1000);
    assert.equal((await command(['browser-record','mark','--out',out,'--note','个人菜单出现，确认登录成功']).done).code,0);
    await delay(400);
    await f.cdp.eval(`(()=>{const button=document.querySelector('iframe').contentDocument.getElementById('frame-button');if(button.textContent!=='Frame action')throw Error('stale fixture');button.click();})()`);
    await delay(400);
    await f.cdp.eval(`(()=>{const link=document.getElementById('next');if(link.textContent!=='Continue')throw Error('stale fixture');link.click();})()`);
    await until(()=>f.cdp.eval(`location.pathname==='/done' && document.readyState==='complete'`),'navigation complete');
    await delay(1000);
    const stop=await command(['browser-record','stop','--out',out]).done;assert.equal(stop.code,0,stop.stderr);
    const result=await recorder.done;assert.equal(result.code,0,`${result.stdout}\n${result.stderr}`);
    const manifest=await json(join(out,'manifest.json'));assert.equal(manifest.status,'complete');assert.equal(manifest.network_redaction,redacted?'enabled':'disabled');
    const steps=await jsonl(join(out,'steps.jsonl'));
    const actions=steps.filter(s=>s.kind==='action');
    assert.deepEqual(actions.slice(0,3).map(s=>s.action.kind),['input','input','click']);
    assert.notEqual(actions[0].action.node_id,actions[1].action.node_id);
    assert.ok(actions.some(s=>s.action.label==='Frame action'));
    assert.ok(actions.some(s=>s.action.label==='Continue'),'navigation-triggering click must survive document destruction');
    assert.ok(steps.some(s=>s.kind==='mark'));
    const network=await jsonl(join(out,'evidence/network.jsonl'));
    const pollsRecorded=network.filter(r=>r.request?.url?.includes('/auth/qr/status'));
    assert.equal(pollsRecorded.reduce((n,r)=>n+r.count,0),4);
    assert.ok(pollsRecorded.some(r=>r.count===3 && r.body.includes('waiting')),'repeated waiting polls should fold');
    assert.ok(pollsRecorded.some(r=>r.body.includes('confirmed')),'HTTP 200 business transition must survive');
    assert.equal(serverRequests.filter(r=>r.url==='/auth/qr/status').length,4);
    assert.ok(serverRequests.some(r=>r.authorization==='Bearer auth-secret' && r.cookie?.includes('cookie-secret')));
    const files=(await readdir(join(out,'evidence'))).filter(n=>n.startsWith('network'));
    const evidence=(await Promise.all(files.map(n=>readFile(join(out,'evidence',n),'utf8')))).join('\n');
    for(const secret of ['query-secret','auth-secret','password-secret','response-secret','cookie-secret']){
      assert.equal(evidence.includes(secret),!redacted,`network redaction=${redacted}, ${secret}`);
    }
    const ui=await readFile(join(out,'evidence/ui.jsonl'),'utf8');assert.ok(ui.includes('canvas'));assert.ok(ui.includes('个人菜单'));
    const images=await readdir(join(out,'images'));assert.ok(images.length>=2 && images.length<12);
    for(const name of images){const image=await readFile(join(out,'images',name));assert.equal(image.subarray(1,4).toString(),'PNG');}
    assert.ok((await readFile(join(out,'walkthrough.md'),'utf8')).includes('扫码登录'));
    assert.ok(!(await readFile(join(out,'walkthrough.md'),'utf8')).includes('document_id'),'the walkthrough should not dump implementation metadata');
    assert.equal((await command(['browser-record','export','--out',out]).done).code,0);
    const before=(await stat(join(out,'steps.jsonl'))).size;
    const overwrite=await command(['browser-record','start','--cdp-port',String(f.port),'--page',f.target.id,'--out',out,'--seconds','1']).done;
    assert.notEqual(overwrite.code,0);assert.equal((await stat(join(out,'steps.jsonl'))).size,before);
    console.log(JSON.stringify({redacted,out,steps:steps.length,network_groups:network.length,poll_requests:4,poll_groups:pollsRecorded.length,screenshots:images.length,gaps:manifest.gaps}));
  }finally{
    if(recorder && recorder.child.exitCode===null){recorder.child.kill('SIGINT');await recorder.done;}
    await f.close();
    if(!process.env.AAD_KEEP_TEST_ARTIFACTS)await rm(root,{recursive:true,force:true});
  }
}
test('browser task recording: raw network default and optional redaction', {skip:!process.env.AAD_TEST_CHROME,timeout:90000},async()=>{
  await runCase(false);await runCase(true);
});

test('CDP disconnect is reported as incomplete, not a successful empty recording', {skip:!process.env.AAD_TEST_CHROME,timeout:20000},async()=>{
  const f=await fixture((req,res)=>{res.setHeader('Content-Type','text/html');res.end('<button>Fixture</button>');});
  const root=await mkdtemp(join(tmpdir(),'aad-recording-disconnect-')),out=join(root,'session');let recorder;
  try {
    recorder=command(['browser-record','start','--cdp-port',String(f.port),'--page',f.target.id,'--out',out,'--no-screenshots']);
    await until(async()=> (await json(join(out,'manifest.json'))).status==='recording','recorder ready');
    await delay(400);
    await f.cdp.call('Target.closeTarget',{targetId:f.target.id});
    const result=await recorder.done;assert.notEqual(result.code,0);
    const manifest=await json(join(out,'manifest.json'));assert.equal(manifest.status,'incomplete');assert.ok(manifest.error.includes('disconnected'));
  }finally{
    if(recorder && recorder.child.exitCode===null){recorder.child.kill('SIGINT');await recorder.done;}
    await f.close();if(!process.env.AAD_KEEP_TEST_ARTIFACTS)await rm(root,{recursive:true,force:true});
  }
});

test('cross-origin frame, related popup lifecycle, unrelated tab isolation and graceful Ctrl+C', {skip:!process.env.AAD_TEST_CHROME,timeout:45000},async()=>{
  const f=await fixture((req,res)=>{
    res.setHeader('Content-Type','text/html; charset=utf-8');
    if(req.url==='/cross')res.end('<button id="cross-action" onclick="this.disabled=true">Cross frame</button>');
    else if(req.url==='/popup')res.end('<button id="close" onclick="setTimeout(()=>window.close(),300)">Close popup</button>');
    else if(req.url==='/unrelated')res.end('<h1>unrelated sentinel</h1>');
    else res.end('<button id="popup" onclick="window.open(\'/popup\',\'login-popup\')">Open popup</button><iframe id="cross"></iframe>');
  });
  const root=await mkdtemp(join(tmpdir(),'aad-recording-lifecycle-')),out=join(root,'session');let recorder;
  try {
    recorder=command(['browser-record','start','--cdp-port',String(f.port),'--page',f.target.id,'--out',out,'--no-screenshots']);
    const manifest=await until(async()=>{const m=await json(join(out,'manifest.json'));return m.status==='recording'?m:false;},'recording ready');
    await f.cdp.call('Target.createTarget',{url:f.url+'unrelated'});
    const cross=f.url.replace('127.0.0.1','localhost')+'cross';
    await f.cdp.eval(`(()=>{const frame=document.getElementById('cross');if(!frame||frame.tagName!=='IFRAME')throw Error('fixture changed');frame.src=${JSON.stringify(cross)};})()`);
    const target=await until(async()=> (await f.cdp.call('Target.getTargets')).targetInfos.find(t=>t.type==='iframe' && t.url===cross),'OOPIF target');
    await until(async()=> (await readFile(join(out,'evidence/ui.jsonl'),'utf8')).includes('Cross frame'),'OOPIF observer installed');
    const attached=await f.cdp.call('Target.attachToTarget',{targetId:target.targetId,flatten:true});
    const response=await f.cdp.call('Runtime.evaluate',{expression:`(()=>{const el=document.getElementById('cross-action');if(el?.textContent!=='Cross frame')throw Error('stale fixture');el.click();return el.disabled;})()`,returnByValue:true},attached.sessionId);
    assert.equal(response.result.value,true);
    await f.cdp.call('Runtime.evaluate',{expression:`(()=>{const el=document.getElementById('popup');if(el?.textContent!=='Open popup')throw Error('stale fixture');el.click();})()`,userGesture:true});
    const popup=await until(async()=> (await f.cdp.call('Target.getTargets')).targetInfos.find(t=>t.type==='page' && t.url===f.url+'popup'),'popup target');
    await until(async()=> (await readFile(join(out,'evidence/ui.jsonl'),'utf8')).includes('Close popup'),'popup observer installed');
    const popupSession=await f.cdp.call('Target.attachToTarget',{targetId:popup.targetId,flatten:true});
    await f.cdp.call('Runtime.evaluate',{expression:`(()=>{const el=document.getElementById('close');if(el?.textContent!=='Close popup')throw Error('stale fixture');el.click();})()`},popupSession.sessionId);
    await delay(800);
    assert.equal(recorder.child.exitCode,null,'closing a related popup must not kill the recorder');
    recorder.child.kill('SIGINT');const result=await recorder.done;assert.equal(result.code,0,`${result.stdout}\n${result.stderr}`);
    const steps=await jsonl(join(out,'steps.jsonl'));
    for(const name of ['Cross frame','Open popup','Close popup'])assert.ok(steps.some(s=>s.action?.label===name),name);
    assert.ok(!JSON.stringify(steps).includes('unrelated sentinel'));
    const frameTree=await f.cdp.call('Page.getFrameTree');
    const world=await f.cdp.call('Page.createIsolatedWorld',{frameId:frameTree.frameTree.frame.id,worldName:'aad-record-'+manifest.session_id});
    const stopped=await f.cdp.call('Runtime.evaluate',{contextId:world.executionContextId,expression:'typeof __aadCdp',returnByValue:true});
    assert.equal(stopped.result.value,'undefined','stop must remove passive listeners');
    assert.equal((await json(join(out,'manifest.json'))).status,'complete');
    console.log(JSON.stringify({lifecycle_out:out,steps:steps.length}));
  }finally{
    if(recorder && recorder.child.exitCode===null){recorder.child.kill('SIGINT');await recorder.done;}
    await f.close();if(!process.env.AAD_KEEP_TEST_ARTIFACTS)await rm(root,{recursive:true,force:true});
  }
});
