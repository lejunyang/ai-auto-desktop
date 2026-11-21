import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {writeFile} from 'node:fs/promises';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {join,resolve} from 'node:path';
import vm from 'node:vm';
import {fixture} from './fixture.mjs';

const source=name=>readFileSync(new URL(`../../crates/aad-browser/src/${name}`,import.meta.url),'utf8');
// Assemble the page script exactly as `cdp::observer_script` does.
const script=source('observer.js').replace('/*AAD_NAMING*/',source('naming.js')).replace('__AAD_BINDING__','');
// The shared naming code reads an element's text through a TreeWalker; this
// fake yields each element's textContent as its single text node.
const NodeFilter={SHOW_ELEMENT:1,SHOW_TEXT:4};
const createTreeWalker=root=>{let done=!root.textContent;return {nextNode:()=>done?null:(done=true,{nodeValue:root.textContent})};};
test('preserve action order, repeated clicks and distinct unlabeled fields; bound the queue',()=>{
  const handlers=new Map();
  const context={document:{addEventListener:(n,f)=>handlers.set(n,f),removeEventListener:()=>{},createTreeWalker},NodeFilter,location:{href:'fixture'},performance,Date,Math};
  vm.runInNewContext(script,context);
  const input=value=>({tagName:'INPUT',type:'text',value,getAttribute:()=>null});
  const first=input('alice'), second=input('bob'), button={tagName:'BUTTON',textContent:'submit',getAttribute:()=>null};
  const event=(name,target)=>handlers.get(name)({target});
  event('input',first);event('click',button);event('input',second);event('click',button);event('click',button);
  const result=context.__aadCdp.takeReport();
  assert.equal(result.events.map(e=>e.kind).join(','),'input,click,input,click,click');
  assert.notEqual(result.events[0].node_id,result.events[2].node_id);
  // Each step carries the role and name the control session replays it by.
  assert.deepEqual(JSON.parse(JSON.stringify(result.events.slice(0,2).map(e=>e.control))),[{role:'textbox',name:''},{role:'button',name:'submit'}]);
  for(let i=0;i<2000;i++)event('input',first);
  assert.equal(context.__aadCdp.takeReport().events.length,1);
  for(let i=0;i<600;i++)event('click',button);
  const overflow=context.__aadCdp.takeReport();
  assert.equal(overflow.events.length,512);assert.equal(overflow.dropped,88);
  context.__aadCdp.stop();assert.equal(context.__aadCdp,undefined);
});

test('file outcomes never inspect file data or use post-selection names',()=>{
  const handlers=new Map();
  const context={document:{addEventListener:(n,f)=>handlers.set(n,f),createTreeWalker},NodeFilter,location:{href:'fixture'},performance};
  vm.runInNewContext(script,context);
  let label='Attachment';
  const files={length:0,get 0(){throw Error('inspected selected file');}};
  const field={tagName:'INPUT',type:'file',get value(){throw Error('read file control value');},files,
    getClientRects:()=>[],getAttribute:key=>key==='aria-label'?label:null};
  const emit=(type,target=field,isTrusted=true)=>handlers.get(type)({target,type,isTrusted});
  emit('click'); files.length=1; label='private.txt';
  emit('input',field,false); emit('change'); emit('input'); emit('click'); emit('cancel');
  field.webkitdirectory=true; emit('input');
  emit('input',{tagName:'INPUT',type:'file',files,getClientRects:()=>[],getAttribute:()=>{throw Error('read unobserved label');}});
  const events=context.__aadCdp.takeReport().events;
  assert.deepEqual(Array.from(events,event=>event.kind),['file_activation','upload','file_activation','file_cancel','file_directory','file_target_unobserved']);
  assert.equal(events[1].control.name,'Attachment');
  assert.ok(!JSON.stringify(events).includes('private.txt'));
});

test('real Chromium reports semantic changes and ordered actions', {skip:!process.env.AAD_TEST_CHROME}, async()=>{
  const f=await fixture((req,res)=>{res.setHeader('Content-Type','text/html; charset=utf-8');res.end('<input id="first"><input id="second"><button id="open" onclick="document.getElementById(\'qr\').hidden=false;this.disabled=true">扫码登录</button><dialog id="qr" open hidden>请使用手机扫码<canvas></canvas></dialog>');});
  try {
    await f.cdp.eval(script);
    const before=await f.cdp.eval('__aadCdp.snapshot()');
    assert.ok(before.nodes.some(n=>n.name==='扫码登录' && n.enabled),JSON.stringify(before));
    // These fixture elements were just observed; recheck each before synthesis.
    await f.cdp.eval(`(()=>{const a=document.getElementById('first'),b=document.getElementById('second'),button=document.getElementById('open');if(!a||!b||button.textContent!=='扫码登录')throw Error('fixture changed');a.value='alice';a.dispatchEvent(new Event('input',{bubbles:true}));b.value='bob';b.dispatchEvent(new Event('input',{bubbles:true}));button.click();})()`);
    const report=await f.cdp.eval('__aadCdp.takeReport()');
    assert.deepEqual(report.events.map(e=>e.kind),['input','input','click']);
    assert.notEqual(report.events[0].node_id,report.events[1].node_id);
    const after=await f.cdp.eval('__aadCdp.snapshot()');
    assert.ok(after.nodes.some(n=>n.name==='扫码登录' && !n.enabled));
    assert.ok(after.nodes.some(n=>n.role==='canvas'));
    await f.cdp.eval(`document.body.insertAdjacentHTML('beforeend','<table><tr aria-label="Order B"><td><button>Save</button></td></tr><tr aria-label="Order A"><td><button>Save</button></td></tr></table>');true`);
    const labels=await f.cdp.eval("Array.from(document.querySelectorAll('tr')).map(row=>row.getAttribute('aria-label'))");
    assert.deepEqual(labels,['Order B','Order A']);
    await f.cdp.eval(`const save=document.querySelector('tr[aria-label="Order A"] button');if(save.textContent==='Save')save.click();true`);
    const report2=await f.cdp.eval('__aadCdp.takeReport()');
    assert.deepEqual(report2.events[0].control.within,{role:'row',name:'Order A'});
    const cli=process.env.AAD_TEST_CLI || resolve('target/debug/aad');
    const run=async args=>JSON.parse((await promisify(execFile)(cli,args,{env:{...process.env,
      AAD_CDP_PORT:String(f.port),AAD_RECORDINGS_DIR:join(f.root,'store'),AAD_CONTROL_LEASE_DIR:join(f.root,'leases')}})).stdout);
    await writeFile(join(f.root,'steps.jsonl'),JSON.stringify({kind:'action',action:report2.events[0]})+'\n');
    const exported=await run(['browser-record','steps','--out',f.root,'--save','row-order']);
    assert.deepEqual(exported.steps[0].locator.within,{role:'row',name:'Order A'});
    await f.cdp.eval(`const rows=document.querySelectorAll('tr');rows[0].before(rows[1]);window.clicked=[];
      for(const row of rows) row.querySelector('button').onclick=()=>clicked.push(row.getAttribute('aria-label'));true`);
    assert.equal((await run(['run','row-order','--permission','desktop.input','--permission','desktop.observe'])).status,'succeeded');
    assert.deepEqual(await f.cdp.eval('clicked'),['Order A']);
    await f.cdp.eval('__aadCdp.stop()');
    assert.equal(await f.cdp.eval('typeof __aadCdp'),'undefined');
  } finally {await f.close();}
});
