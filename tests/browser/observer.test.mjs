import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
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
    await f.cdp.eval('__aadCdp.stop()');
    assert.equal(await f.cdp.eval('typeof __aadCdp'),'undefined');
  } finally {await f.close();}
});
