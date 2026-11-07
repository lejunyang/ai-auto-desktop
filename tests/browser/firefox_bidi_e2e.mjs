// Linux acceptance: node >=22, AAD_TEST_FIREFOX and AAD_TEST_AAD absolute paths.
// All input uses the public control CLI. A private HTTP fixture reports actual
// DOM/event state independently of the provider's helper and snapshots.
import {spawn} from 'node:child_process';
import {mkdtemp,rm,mkdir,writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join,resolve} from 'node:path';
import {createServer} from 'node:http';
import {once} from 'node:events';
import {createInterface} from 'node:readline';
import assert from 'node:assert/strict';
if(process.platform!=='linux')throw Error('This fixture currently validates Linux Firefox only');
const firefoxPath=process.env.AAD_TEST_FIREFOX;
const aadPath=process.env.AAD_TEST_AAD;
if(!firefoxPath||!aadPath)throw Error('Set AAD_TEST_FIREFOX and AAD_TEST_AAD to absolute executable paths');
const out=resolve(process.env.AAD_BIDI_ARTIFACTS||'artifacts/capability-completion/firefox-bidi/provider');
await mkdir(out,{recursive:true});
const profile=await mkdtemp(join(tmpdir(),'aad-bidi-e2e-'));
let browserState={},command='',firefox,control,stderr='',aadStderr='',sequence=0,session;
const pending=new Map(),transcript=[],results=[];
const navigationStates=new Map();
let navigationReports=0;
const html=`<!doctype html><meta charset=utf-8><title>AAD Firefox fixture</title>
<input id=editor aria-label=Editor><button id=save aria-label=Save>Save</button>
<button id=hover aria-label=Hover>Hover</button>
<button id=pointer aria-label=Pointer>Pointer</button>
<select id=size aria-label=Size><option>Small</option><option>Large</option></select>
<input id=agree type=checkbox aria-label=Agree>
<div aria-label="Scope region"><button aria-label=Scoped>Scoped</button></div>
<div id=panel aria-label=Panel style="overflow:auto;width:240px;height:45px"><div style="height:300px">rows</div></div>
<div id=covered style="position:relative;width:200px"><button aria-label=Covered>Covered</button></div>
<input id=password type=password value=synthetic-light-private>
<x-closed></x-closed><iframe src=/frame style="width:240px;height:35px"></iframe>
<script>
const closed=document.querySelector('x-closed').attachShadow({mode:'closed'});
closed.innerHTML='<input type=password value=synthetic-closed-first><input type=password value=synthetic-closed-second>';
let clicks=0,clickedTrusted=null,hovered=false,held=new Set(),keys=0,ups=0,lastCommand='',pointerButtons=[],doubles=0;
let coverNext=false;const keyTimes=new Map(),keyDurations=[],mouseTimes=new Map(),mouseDurations=[],mouseHeld=new Set();
document.querySelector('#save').onclick=e=>{clicks++;clickedTrusted=e.isTrusted};
document.querySelector('#hover').onmouseenter=e=>hovered=e.isTrusted;
document.querySelector('#pointer').onmousedown=e=>{pointerButtons.push([e.button,e.isTrusted]);if(e.button)e.preventDefault();if(coverNext){coverNext=false;const r=e.currentTarget.getBoundingClientRect(),cover=document.createElement('div');cover.id='overlay';cover.style='position:fixed;z-index:10000;background:white;left:'+r.left+'px;top:'+r.top+'px;width:'+r.width+'px;height:'+r.height+'px';document.body.append(cover)}};
document.querySelector('#pointer').oncontextmenu=e=>e.preventDefault();document.querySelector('#pointer').ondblclick=e=>{if(e.isTrusted)doubles++};
document.addEventListener('keydown',e=>{held.add(e.key);keys++;keyTimes.set(e.key,performance.now())});document.addEventListener('keyup',e=>{held.delete(e.key);ups++;keyDurations.push([e.key,performance.now()-keyTimes.get(e.key)])});
document.addEventListener('mousedown',e=>{mouseHeld.add(e.button);mouseTimes.set(e.button,performance.now())},true);
document.addEventListener('mouseup',e=>{mouseHeld.delete(e.button);mouseDurations.push([e.button,performance.now()-mouseTimes.get(e.button)])},true);
async function tick(){try{
const next=await(await fetch('/command')).text();
if(next==='replace'){const old=document.querySelector('#editor');old.replaceWith(old.cloneNode(true))}
if(next==='cover'){const cover=document.createElement('div');cover.id='overlay';cover.style='position:absolute;inset:0;background:white';document.querySelector('#covered').append(cover)}
if(next==='reveal'){for(const el of [document.querySelector('#password'),...closed.querySelectorAll('input')]){el.type='text';document.body.append(el)}}
if(next==='remove'){for(const el of [...document.querySelectorAll('input:not(#editor):not(#agree)'),...document.querySelectorAll('iframe,x-closed,#overlay')])el.remove()}
if(next==='navigate'){location.href='/next';return}
if(next==='focus-away')document.querySelector('#hover').focus();
if(next==='cover-next')coverNext=true;
if(next==='uncover')document.querySelector('#overlay')?.remove();
if(next)lastCommand=next;
const editor=document.querySelector('#editor');
await fetch('/observed',{method:'POST',body:JSON.stringify({lastCommand,value:editor.value,start:editor.selectionStart,end:editor.selectionEnd,
clicks,clickedTrusted,hovered,keys,ups,held:[...held],pointerButtons,doubles,keyDurations,mouseDurations,mouseHeld:[...mouseHeld],size:document.querySelector('#size').value,
checked:document.querySelector('#agree').checked,scroll:document.querySelector('#panel').scrollTop})});
}catch{}setTimeout(tick,10)}tick();
</script>`;
const server=createServer(async(req,res)=>{
  if(req.url==='/nav-state'){let bytes='';for await(const b of req)bytes+=b;const state=JSON.parse(bytes);state.sequence=++navigationReports;navigationStates.set(state.path,state);res.end('ok');return}
  if(req.url==='/observed'){let bytes='';for await(const b of req)bytes+=b;browserState=JSON.parse(bytes);res.end('ok');return}
  if(req.url==='/command'){const next=command;command='';res.end(next);return}
  res.writeHead(200,{'Content-Type':'text/html; charset=utf-8'});
  if(req.url.startsWith('/nav-')){res.end(`<!doctype html><title>Navigation fixture</title><button>Navigation</button><button id=long>Long history</button><script>
const token=crypto.randomUUID();async function report(){try{await fetch('/nav-state',{method:'POST',body:JSON.stringify({path:location.pathname,token,visibility:document.visibilityState,focused:document.hasFocus()})})}catch{}setTimeout(report,20)}report();
if(location.pathname==='/nav-guard')document.addEventListener('visibilitychange',()=>{if(document.hidden){const body=document.createElement('body');body.innerHTML='<button>Replaced</button>';document.body.replaceWith(body)}});
document.querySelector('#long').onclick=()=>{history.pushState(null,'','/nav-long?'+('x'.repeat(2600)));history.pushState(null,'','/nav-after-long')};
</script>`);return}
  res.end(req.url==='/frame'?'<title>child</title><input type=password value=synthetic-frame-private><button>Child</button>':req.url==='/next'?'<title>Next</title><button>Next</button>':html);
});
server.listen(0,'127.0.0.1');await once(server,'listening');
const url=`http://127.0.0.1:${server.address().port}/`;
const delay=ms=>new Promise(r=>setTimeout(r,ms));
async function wait(predicate,label,ms=5000){const until=Date.now()+ms;while(!predicate()){if(Date.now()>until)throw Error('Timed out: '+label);await delay(20)}}
function send(operation,timeout=12000){
  const request_id=String(++sequence),request={request_id,session_id:session,timeout_ms:timeout,operation};
  const promise=new Promise((ok,fail)=>{const timer=setTimeout(()=>{pending.delete(request_id);fail(Error('Control timeout: '+operation.method))},timeout+5000);pending.set(request_id,{ok:r=>{clearTimeout(timer);ok(r)},fail})});
  transcript.push({request});control.stdin.write(JSON.stringify(request)+'\n');return {request_id,promise};
}
async function call(operation){const r=await send(operation).promise;if(r.error)throw Error(JSON.stringify(r.error));return r.data}
const observe=resource_id=>call({method:'observe',resource_id,max_nodes:500,view:{mode:'nodes'}});
const find=(o,locator)=>call({method:'find',observation_id:o.observation_id,locator,expect:'one'}).then(r=>r.target);
const findName=(o,name)=>find(o,{name});
async function perform(rid,name,action){const o=await observe(rid),target=await findName(o,name);return call({method:'act',target,action})}
async function pageAction(rid,action){const o=await observe(rid),target=await find(o,{role:'document'});return call({method:'act',target,action})}
async function mutate(value){command=value;await wait(()=>browserState.lastCommand===value,value)}
function record(name){results.push(name);console.log('PASS '+name)}
try{
  firefox=spawn(firefoxPath,['--headless','--no-remote','--profile',profile,'--remote-debugging-port','0',url],{detached:true,stdio:['ignore','ignore','pipe']});
  firefox.stderr.on('data',b=>stderr+=b);
  await wait(()=>/WebDriver BiDi listening on ws:\/\/127\.0\.0\.1:\d+/.test(stderr),'Firefox start',30000);
  const port=stderr.match(/WebDriver BiDi listening on ws:\/\/127\.0\.0\.1:(\d+)/)[1];
  control=spawn(aadPath,['control','serve','--allow-input','--provider','browser.firefox_bidi','--browser-bidi-port',port],{stdio:['pipe','pipe','pipe']});
  control.stderr.on('data',b=>aadStderr+=b);
  createInterface({input:control.stdout}).on('line',line=>{const r=JSON.parse(line);session=r.session_id;const logged=structuredClone(r);if(logged.data?.data)logged.data.data='<image omitted>';transcript.push({response:logged});const p=pending.get(r.request_id);if(p){pending.delete(r.request_id);p.ok(r)}});
  await call({method:'capabilities'});
  const resources=(await call({method:'targets'})).resources;
  const page=resources.find(r=>r.provider==='browser.firefox_bidi'&&r.metadata.url===url&&r.metadata.main_frame);
  assert.ok(page,JSON.stringify(resources));const rid=page.resource_id;
  let o=await observe(rid);
  for(const secret of ['synthetic-light-private','synthetic-closed-first','synthetic-closed-second'])assert.ok(!JSON.stringify(o).includes(secret));
  assert.ok(o.nodes.some(n=>n.states.protected&&n.value===null&&n.actions.length===0));
  record('protected values and inline script source absent from observation');
  const child=resources.find(r=>r.provider==='browser.firefox_bidi'&&!r.metadata.main_frame);
  assert.ok(child);assert.deepEqual(child.capabilities,[]);
  const childObservation=await observe(child.resource_id);assert.ok(childObservation.nodes.every(n=>n.actions.length===0));
  assert.ok(!JSON.stringify(childObservation).includes('synthetic-frame-private'));
  record('child contexts read-only with protected values removed');
  await perform(rid,'Editor',{type:'fill',value:'Firefox 中文'});await wait(()=>browserState.value==='Firefox 中文','fill effect');
  await perform(rid,'Editor',{type:'key',keys:['ctrl','a']});await wait(()=>browserState.start===0&&browserState.end==='Firefox 中文'.length,'real selection');
  await perform(rid,'Editor',{type:'type_text',text:'AZ',mode:'keys'});await wait(()=>browserState.value==='AZ','real key input');
  record('DOM fill, trusted Select All and typed text independently read back');
  await perform(rid,'Save',{type:'click'});await wait(()=>browserState.clicks===1&&browserState.clickedTrusted===true,'trusted click');
  await perform(rid,'Hover',{type:'hover'});await wait(()=>browserState.hovered,'hover');
  await perform(rid,'Pointer',{type:'click',button:'right',fraction:{fx:0.25,fy:0.75}});
  await perform(rid,'Pointer',{type:'click',button:'middle'});
  await wait(()=>browserState.pointerButtons.length===2,'right and middle clicks');assert.deepEqual(browserState.pointerButtons,[[2,true],[1,true]]);
  record('right/middle clicks and element fraction use trusted pointer input');
  await perform(rid,'Size',{type:'select',option:'Large'});await wait(()=>browserState.size==='Large','select');
  await perform(rid,'Agree',{type:'check',checked:true});await wait(()=>browserState.checked,'check');
  await perform(rid,'Panel',{type:'scroll',direction:'down',notches:2});await wait(()=>browserState.scroll>0,'scroll');
  await perform(rid,'Save',{type:'invoke'});await wait(()=>browserState.clicks===2&&browserState.clickedTrusted===false,'DOM invoke');
  record('click/hover/select/check/scroll/invoke actual effects');
  o=await observe(rid);const save=await findName(o,'Save');
  const shot=await call({method:'capture',target:save});
  assert.ok(shot.width>5&&shot.height>5);await writeFile(join(out,'save.png'),Buffer.from(shot.data,'base64'));
  const document=await find(o,{role:'document'});
  assert.equal((await send({method:'capture',target:document}).promise).error?.code,'CONTROL.ACTION_UNSUPPORTED');
  record('safe element PNG captured, protected document capture refused');
  const region=await findName(o,'Scope region');
  const scoped=await call({method:'observe',resource_id:rid,max_nodes:50,view:{mode:'nodes',scope_target:region}});
  assert.ok(scoped.nodes.some(n=>n.name==='Scoped'));assert.ok(!scoped.nodes.some(n=>n.name==='Save'));
  record('scope from observed container without HTML id');
  o=await observe(rid);const oldEditor=await findName(o,'Editor');await mutate('replace');
  assert.equal((await send({method:'act',target:oldEditor,action:{type:'fill',value:'must not be written'}}).promise).error?.code,'CONTROL.STALE_TARGET');
  await mutate('cover');o=await observe(rid);const covered=await find(o,{role:'button',name:'Covered'});
  assert.equal((await send({method:'act',target:covered,action:{type:'click'}}).promise).error?.code,'CONTROL.OCCLUDED');
  record('replaced identity and occluded pointer rejected');
  o=await observe(rid);let editor=await findName(o,'Editor');
  for(const action of [{type:'key',keys:['ctrl','s']},{type:'click',modifiers:['ctrl']},{type:'click',count:2},{type:'type_text',text:'unsafe mode',mode:'unicode'},{type:'hover',ms:100}]){
    o=await observe(rid);editor=await findName(o,'Editor');
    const r=await send({method:'act',target:editor,action}).promise;assert.equal(r.error?.code,'CONTROL.ACTION_UNSUPPORTED');
  }
  record('unsupported shortcuts, pointer modifiers, double click and text mode rejected');
  await mutate('reveal');o=await observe(rid);
  for(const secret of ['synthetic-light-private','synthetic-closed-first','synthetic-closed-second'])assert.ok(!JSON.stringify(o).includes(secret));
  assert.equal((await send({method:'capture',target:await find(o,{role:'document'})}).promise).error?.code,'CONTROL.ACTION_UNSUPPORTED');
  assert.ok(o.nodes.filter(n=>n.states.protected&&n.value===null).length>=3);
  record('all closed/light password identities remain private after reveal and movement');
  await mutate('remove');o=await observe(rid);
  const recovered=await call({method:'capture',target:await find(o,{role:'document'})});assert.ok(recovered.width>100);
  await writeFile(join(out,'after-removal.png'),Buffer.from(recovered.data,'base64'));
  record('capture restored after protected fields and frame removal');
  o=await observe(rid);editor=await findName(o,'Editor');const before=browserState.keys;
  const typing=send({method:'act',target:editor,action:{type:'type_text',text:'x'.repeat(512),mode:'keys'}});
  await wait(()=>browserState.keys>before,'typing began');
  await call({method:'cancel',request_id:typing.request_id});
  const cancelled=await typing.promise;assert.equal(cancelled.error?.code,'CONTROL.CANCELLED');
  await wait(()=>browserState.held.length===0,'keys released');const length=browserState.value.length;await delay(200);assert.equal(browserState.value.length,length);assert.ok(length<512);
  await perform(rid,'Save',{type:'click'});await wait(()=>browserState.clicks===3,'input after cancel');
  record('in-flight cancellation stops typing, releases keys and permits next action');
  await perform(rid,'Editor',{type:'key',keys:['ArrowRight'],hold_ms:200});
  await wait(()=>browserState.keyDurations.some(([key,ms])=>key==='ArrowRight'&&ms>=180),'key held for requested duration');
  await perform(rid,'Pointer',{type:'click',hold_ms:200});
  await wait(()=>browserState.mouseDurations.at(-1)?.[1]>=180,'pointer held for requested duration');
  assert.ok(browserState.mouseDurations.at(-1)[1]<2000);assert.deepEqual(browserState.held,[]);assert.deepEqual(browserState.mouseHeld,[]);
  record('key and click hold durations independently measured by DOM events');
  o=await observe(rid);editor=await findName(o,'Editor');let startKeys=browserState.keys;
  const noBudget=await send({method:'act',target:editor,action:{type:'key',keys:['ArrowRight'],hold_ms:5000}},500).promise;
  assert.equal(noBudget.error?.code,'CONTROL.TIMEOUT');await delay(100);assert.equal(browserState.keys,startKeys);
  record('insufficient hold budget rejected before key down');
  o=await observe(rid);editor=await findName(o,'Editor');
  const focusHold=send({method:'act',target:editor,action:{type:'key',keys:['ArrowRight'],hold_ms:5000}});
  await wait(()=>browserState.held.includes('ArrowRight'),'held key pressed');await mutate('focus-away');
  const focusFailure=await focusHold.promise;assert.equal(focusFailure.error?.code,'CONTROL.FOCUS_FAILED');assert.equal(focusFailure.error.details.residual_state_cleared,true);
  await wait(()=>browserState.held.length===0,'held key released after focus loss');
  await mutate('cover-next');o=await observe(rid);let pointer=await findName(o,'Pointer');
  const occluded=await send({method:'act',target:pointer,action:{type:'click',hold_ms:5000}}).promise;
  assert.equal(occluded.error?.code,'CONTROL.OCCLUDED');assert.equal(occluded.error.details.residual_state_cleared,true);
  await wait(()=>browserState.mouseHeld.length===0,'held mouse released after overlay');await mutate('uncover');
  record('focus loss and pointer occlusion during hold stop and release input');
  o=await observe(rid);pointer=await findName(o,'Pointer');
  const mouseHold=send({method:'act',target:pointer,action:{type:'click',hold_ms:5000}});
  await wait(()=>browserState.mouseHeld.includes(0),'held mouse pressed');await call({method:'cancel',request_id:mouseHold.request_id});
  const mouseCancelled=await mouseHold.promise;assert.equal(mouseCancelled.error?.code,'CONTROL.CANCELLED');assert.equal(mouseCancelled.error.details.residual_state_cleared,true);
  await wait(()=>browserState.mouseHeld.length===0,'held mouse released after cancel');assert.ok(browserState.mouseDurations.at(-1)[1]<4000);
  await perform(rid,'Pointer',{type:'click'});
  record('held pointer cancellation releases early and allows subsequent input');
  o=await observe(rid);const oldSave=await findName(o,'Save');command='navigate';await delay(300);
  assert.equal((await send({method:'act',target:oldSave,action:{type:'click'}}).promise).error?.code,'CONTROL.STALE_TARGET');
  record('new document cannot reuse old isolated realm or observation');
  await pageAction(rid,{type:'navigate',url:url+'nav-first'});
  await wait(()=>navigationStates.has('/nav-first'),'navigation destination independently loaded');
  const beforeReload=navigationStates.get('/nav-first').token;
  await pageAction(rid,{type:'reload'});
  await wait(()=>navigationStates.get('/nav-first')?.token!==beforeReload,'reload created new document');
  record('anchored navigation and reload independently report new document');
  for(const action of [{type:'navigate',url:'file:///ungranted/aad-fixture.html'},{type:'open_tab',url:'file:///ungranted/aad-fixture.html'}]){
    o=await observe(rid);const document=await find(o,{role:'document'});
    assert.equal((await send({method:'act',target:document,action}).promise).error?.code,'POLICY.DENIED');
  }
  const unchanged=(await call({method:'targets'})).resources.filter(r=>r.metadata.main_frame);
  assert.equal(unchanged.length,1);assert.equal(unchanged[0].metadata.url,url+'nav-first');
  o=await observe(rid);const ordinaryButton=await find(o,{role:'button',name:'Navigation'});
  assert.equal((await send({method:'act',target:ordinaryButton,action:{type:'reload'}}).promise).error?.code,'CONTROL.ACTION_UNSUPPORTED');
  record('ungranted destinations and non-document navigation anchors rejected');
  const opened=await pageAction(rid,{type:'open_tab',url:url+'nav-second'});
  const openedId=opened.backend_result.page_id;
  await wait(()=>navigationStates.has('/nav-second'),'new tab destination');
  const tabs=(await call({method:'targets'})).resources;
  const second=tabs.find(r=>r.metadata.context===openedId);assert.ok(second);assert.notEqual(second.resource_id,rid);
  await wait(()=>navigationStates.get('/nav-first').visibility==='hidden'&&navigationStates.get('/nav-second').visibility==='visible','new tab foreground');
  await pageAction(rid,{type:'activate_page'});
  await wait(()=>navigationStates.get('/nav-first').visibility==='visible'&&navigationStates.get('/nav-second').visibility==='hidden','observed page activated');
  record('new tab loaded and explicit activation verified through page visibility');
  await pageAction(second.resource_id,{type:'close_page'});
  assert.ok(!(await call({method:'targets'})).resources.some(r=>r.metadata.context===openedId));
  assert.equal((await observe(rid)).metadata.url,url+'nav-first');
  record('observed tab closed while source document remains available');
  await pageAction(rid,{type:'navigate',url:url+'nav-guard'});
  o=await observe(rid);const guard=await find(o,{role:'document'});
  const interruptedOpen=await send({method:'act',target:guard,action:{type:'open_tab',url:url+'nav-must-not-load'}}).promise;
  assert.equal(interruptedOpen.error?.code,'CONTROL.STALE_TARGET');
  const createdId=interruptedOpen.error.details.created_page_id;assert.ok(createdId);
  assert.ok(!navigationStates.has('/nav-must-not-load'));
  const partial=(await call({method:'targets'})).resources.find(r=>r.metadata.context===createdId);
  assert.ok(partial);assert.equal(partial.metadata.url,'about:blank');
  await pageAction(partial.resource_id,{type:'close_page'});
  record('tab creation rechecks changed source before destination navigation and reports partial effect');
  await pageAction(rid,{type:'navigate',url:url+'nav-history-a'});
  await pageAction(rid,{type:'navigate',url:url+'nav-history-b'});
  let beforeHistory=navigationReports;
  await pageAction(rid,{type:'back'});
  await wait(()=>navigationStates.get('/nav-history-a')?.sequence>beforeHistory,'back restored exact same-origin document');
  assert.equal((await observe(rid)).metadata.url,url+'nav-history-a');
  beforeHistory=navigationReports;
  await pageAction(rid,{type:'forward'});
  await wait(()=>navigationStates.get('/nav-history-b')?.sequence>beforeHistory,'forward restored exact same-origin document');
  assert.equal((await observe(rid)).metadata.url,url+'nav-history-b');
  record('same-origin back and forward restore independently reporting documents');
  await perform(rid,'Long history',{type:'click'});
  await wait(()=>navigationStates.has('/nav-after-long'),'page-created history entries');
  beforeHistory=navigationReports;
  await pageAction(rid,{type:'back'});
  await wait(()=>navigationStates.get('/nav-long')?.sequence>beforeHistory,'long URL history entry');
  assert.equal((await observe(rid)).metadata.url,url+'nav-long?'+'x'.repeat(2600));
  await pageAction(rid,{type:'forward'});
  assert.equal((await observe(rid)).metadata.url,url+'nav-after-long');
  record('existing long HTTP history URLs avoid explicit navigation length gate');
  for(const next of [url.replace('127.0.0.1','localhost')+'nav-foreign',url+'nav-return']){
    await pageAction(rid,{type:'navigate',url:next});
    o=await observe(rid);const body=await find(o,{role:'document'});
    assert.ok(!o.nodes.find(n=>n.node_id===body.node_id).actions.includes('back'));
    assert.equal((await send({method:'act',target:body,action:{type:'back'}}).promise).error?.code,'CONTROL.ACTION_UNSUPPORTED');
    assert.equal((await observe(rid)).metadata.url,next);
  }
  record('foreign history boundaries are refused without skipping to an older same-origin entry');
}catch(error){results.push({failure:String(error),browserState});console.error(error);process.exitCode=1}
finally{
  if(control){try{await call({method:'close'})}catch{}control.stdin.end();await Promise.race([once(control,'exit'),delay(3000)]);if(control.exitCode===null)control.kill('SIGKILL')}
  if(firefox){try{process.kill(-firefox.pid,'SIGTERM')}catch{}await delay(200);try{process.kill(-firefox.pid,'SIGKILL')}catch{}}
  server.closeAllConnections();server.close();await rm(profile,{recursive:true,force:true});
  await writeFile(join(out,'results.json'),JSON.stringify(results,null,2));await writeFile(join(out,'control.ndjson'),transcript.map(r=>JSON.stringify(r)).join('\n')+'\n');
  await writeFile(join(out,'firefox-stderr.log'),stderr);await writeFile(join(out,'control-stderr.log'),aadStderr);
}
