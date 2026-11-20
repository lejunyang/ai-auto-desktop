import assert from 'node:assert/strict';
import {spawn, execFile} from 'node:child_process';
import {once} from 'node:events';
import {readFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {createInterface} from 'node:readline';
import {setTimeout as delay} from 'node:timers/promises';
import {test} from 'node:test';
import {promisify} from 'node:util';
import {fixture, connect} from '../browser/fixture.mjs';

const exec = promisify(execFile);
async function until(read, label) {
  for (let i=0; i<150; i++) {
    const value = await read();
    if (value) return value;
    await delay(50);
  }
  throw Error('Timed out waiting for '+label);
}

test('one live capture replays browser, main window and a late dialog', {timeout:90000}, async t => {
  const effects = [];
  const page = await fixture((req,res)=>{
    if (req.url.startsWith('/effect/')) {
      effects.push(decodeURIComponent(req.url.slice(8)));
      res.end('ok');
      return;
    }
    res.setHeader('Content-Type','text/html');
    res.end('<input aria-label="Order" oninput="fetch(\'/effect/web=\'+this.value)"><button onclick="fetch(\'/effect/finish\')">Finish</button>');
  });
  t.after(()=>page.close());
  const out=join(page.root,'capture');
  const cli=process.env.AAD_TEST_CLI || resolve('target/debug/aad');
  const env={...process.env,AAD_CDP_PORT:String(page.port),AAD_CONTROL_LEASE_DIR:join(page.root,'leases')};
  const run=async (...args)=>{
    try {return JSON.parse((await exec(cli,['--compact',...args],{env,maxBuffer:4194304})).stdout);}
    catch (err) {throw Error([err.message,err.stdout,err.stderr].join('\n'));}
  };
  async function native(companion=false) {
    const child=spawn(companion ? process.env.AAD_MIXED_COMPANION : process.env.AAD_MIXED_FIXTURE,companion ? ['ledger'] : [],{stdio:['ignore','pipe','inherit'],env});
    const lines=createInterface({input:child.stdout});
    t.after(()=>child.kill());
    lines.on('line',s=>{if(s!=='READY')effects.push(s);});
    assert.deepEqual(await once(lines,'line'),['READY']);
    const resource=await until(async()=> (await run('apps')).apps.find(a=>a.metadata.process_id===child.pid),'AT-SPI application');
    return {child,resource};
  }
  let {child,resource}=await native();
  let ledger=await native(true);
  async function start(resource,dir) {
    const args=['browser-record','mixed','start','--native',resource.resource_id,'--window','Invoice',
      '--dialog','Approval','--page',page.target.id,'--cdp-port',String(page.port),'--out',dir];
    args.push('--also-native',ledger.resource.resource_id,'Ledger');
    const recording=spawn(cli,args,{env,stdio:['ignore','pipe','pipe']});
    t.after(()=>recording.kill());
    let log='';
    recording.stderr.on('data',s=>{log+=s;});
    recording.stdout.on('data',s=>{log+=s;});
    const ended=once(recording,'exit');
    await until(()=>{
      if(recording.exitCode!==null) throw Error(log);
      return log.includes('Mixed recording ready');
    },'both recorders');
    return async ()=>{
      await run('browser-record','mixed','stop','--out',dir);
      assert.equal((await ended)[0],0,log);
      return JSON.parse(await readFile(join(dir,'capture.json')));
    };
  }
  const stop=await start(resource,out);

  const type=async (title,text)=>{
    const {stdout}=await exec('xdotool',['search','--onlyvisible','--name','^'+title+'$']);
    const window=stdout.trim().split('\n').at(-1);
    await exec('xdotool',['windowactivate','--sync',window,'type','--clearmodifiers',text]);
    await until(()=>effects.includes(title+'='+text),'native '+title);
    await delay(80); // Distinct user gestures outside the clock-alignment interval.
  };
  const web=async (text,expected)=>{
    await page.cdp.eval('document.querySelector("input").focus()');
    await page.cdp.call('Input.insertText',{text});
    await until(()=>effects.includes('web='+expected),'web edit');
    await delay(80);
  };
  await web('W','W');
  await type('Unrelated','Z');
  await type('Invoice','N');
  await type('Ledger','L');
  await web('X','WX');
  await type('Approval','D');

  // An independent tab receives trusted input, and script-generated clicks on
  // the selected tab are visible to the fixture. Neither belongs in replay.
  const other=await page.cdp.call('Target.createTarget',{url:page.url+'elsewhere'});
  const otherInfo=await until(async()=> (await (await fetch('http://127.0.0.1:'+page.port+'/json/list')).json()).find(p=>p.id===other.targetId),'other target');
  const second=await connect(otherInfo.webSocketDebuggerUrl);
  try {
    await until(()=>second.eval('!!document.querySelector("input")'),'other input');
    await second.eval('document.querySelector("input").focus()');
    await second.call('Input.insertText',{text:'OTHER'});
    await until(()=>effects.includes('web=OTHER'),'other tab effect');
  } finally {
    second.close();
    await page.cdp.call('Target.closeTarget',{targetId:other.targetId});
  }
  await page.cdp.eval('document.querySelector("button").click()');
  await until(()=>effects.includes('finish'),'untrusted click effect');
  const point=await page.cdp.eval('(()=>{const r=document.querySelector("button").getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2}})()');
  for(const type of ['mousePressed','mouseReleased']) await page.cdp.call('Input.dispatchMouseEvent',{type,button:'left',clickCount:1,...point});
  await until(()=>effects.filter(e=>e==='finish').length===2,'trusted click effect');
  assert.deepEqual(effects,['web=W','Unrelated=Z','Invoice=N','Ledger=L','web=WX','Approval=D','web=OTHER','finish','finish']);
  const report=await stop();
  t.diagnostic(JSON.stringify(report));
  assert.equal(report.status,'complete');
  assert.ok(report.filtered.other_native_window>=1);
  assert.ok(report.filtered.untrusted_dom>=1);
  assert.equal(report.actions,6);
  assert.equal(report.native_sources.length,2);

  const closed=once(child,'exit');
  child.kill(); await closed;
  const closedLedger=once(ledger.child,'exit');
  ledger.child.kill(); await closedLedger;
  const fresh=await native();
  ledger=await native(true);
  await page.cdp.call('Page.reload');
  await until(()=>page.cdp.eval('document.readyState==="complete" && document.querySelector("input")?.value===""'),'clean page');
  effects.length=0;
  const result=await run('run',report.workflow,'--permission','desktop.observe','--permission','desktop.input');
  await until(()=>effects.includes('finish'),'replayed browser click');
  assert.deepEqual(effects,['web=W','Invoice=N','Ledger=L','web=WX','Approval=D','finish']);
  assert.equal(await page.cdp.eval('document.querySelector("input").value'),'WX');
  t.diagnostic(JSON.stringify({executed:result.executed_steps,replay:effects}));

  const endGap=await start(fresh.resource,join(page.root,'gap'));
  await page.cdp.eval('document.querySelector("input").select()');
  await page.cdp.call('Input.insertText',{text:'x'.repeat(501)});
  await until(()=>effects.includes('web='+'x'.repeat(501)),'full typed value');
  for(const type of ['mousePressed','mouseReleased']) await page.cdp.call('Input.dispatchMouseEvent',{type,button:'left',clickCount:1,...point});
  const gap=await endGap();
  assert.equal(gap.status,'needs_review');
  assert.equal(gap.actions,1);
  assert.ok(gap.gaps.some(g=>g.source==='browser' && g.skipped.summary==='input'));
  const artifact=JSON.parse(await readFile(gap.workflow));
  assert.deepEqual(artifact.metadata.annotations.capture.gaps,gap.gaps);
});
