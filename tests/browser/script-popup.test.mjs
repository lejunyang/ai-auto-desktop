import assert from 'node:assert/strict';
import {execFile, spawn} from 'node:child_process';
import {writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {createInterface} from 'node:readline';
import {test} from 'node:test';
import {setTimeout as delay} from 'node:timers/promises';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

test('Chromium script binds one new popup and returns to its exact opener', {
  skip: !process.env.AAD_TEST_CHROME, timeout: 60000
}, async t => {
  const browser = await fixture((req, res) => {
    res.setHeader('Content-Type', 'text/html');
    if (req.url === '/consent') return res.end('<!doctype html><button onclick="opener.document.querySelector(\'input\').value=\'approved\';window.close()">Allow</button>');
    res.end(`<!doctype html><style>button,iframe{display:block}</style><input aria-label="Receipt" value="pending">
      <button onclick="setTimeout(()=>window.open('/consent','_blank'),100)">Authorize</button>
      <button onclick="window.open('/consent','_blank');window.open('/consent','_blank')">Ambiguous</button>
      <button onclick="window.open('/consent','_blank','noopener')">No opener</button>
      <button onclick="window.waiting=true">No popup</button>
      <button onclick="document.querySelector('iframe').contentWindow.frameOpen()">From frame</button>
      <script>window.frameOpen=()=>window.open('/consent')</script>
      ${req.url==='/' ? '<iframe src="/frame" style="display:block;width:90vw;height:180px;border:0"></iframe>' : ''}`);
  }, ['--disable-popup-blocking','--enable-automation']);
  t.after(() => browser.close());
  const cli = process.env.AAD_TEST_CLI || resolve('target/debug/aad');
  const env = {...process.env, AAD_CONTROL_LEASE_DIR:join(browser.root,'leases')};
  const url = browser.url+'consent';
  const source = `page.openPopup({name:'Authorize'}, inputs.url);
    const allow={role:'button',name:'Allow'};
    page.wait(allow); page.click(allow); page.returnToOpener(); return page.read({name:'Receipt'});`;
  const file = join(browser.root,'authorize.js');
  await writeFile(file, source);
  await browser.cdp.eval("window.prior=open('/consent','_blank');true");
  await browser.cdp.call('Page.bringToFront');
  const run = promisify(execFile);
  const args = ['browser','script',file,'--url',browser.url,'--browser-port',String(browser.port),
    '--inputs',JSON.stringify({url})];
  await assert.rejects(run(cli,args,{env}), err=>JSON.parse(err.stdout).error.code==='POLICY.DENIED');
  const report = JSON.parse((await run(cli,[...args,'--allow-input'],{env})).stdout);
  assert.equal(report.result,'approved');
  assert.equal(report.completed_actions,2);
  assert.equal(await browser.cdp.eval('prior.closed'),false);

  function mcp(extra=[]) {
    const child=spawn(cli,['mcp','--browser-port',String(browser.port),'--provider','browser.cdp','--allow-input',...extra],
      {stdio:['pipe','pipe','inherit'], env});
    t.after(()=>{child.stdin.end();child.kill();});
    const lines=createInterface({input:child.stdout})[Symbol.asyncIterator]();
    let id=0;
    const tool=async (name,args,error=false)=>{
      child.stdin.write(JSON.stringify({jsonrpc:'2.0',id:++id,method:'tools/call',params:{name,arguments:args}})+'\n');
      const reply=JSON.parse((await lines.next()).value).result;
      assert.equal(reply.isError,error,JSON.stringify(reply));
      return JSON.parse(reply.content[0].text);
    };
    tool.cancel=()=>child.stdin.write(JSON.stringify({jsonrpc:'2.0',method:'notifications/cancelled',params:{requestId:id}})+'\n');
    return tool;
  }
  const tool=mcp();
  const resources=(await tool('control_targets',{})).data.resources;
  const frame=resources.find(r=>r.metadata.url===browser.url+'frame');
  const main=resources.find(r=>r.metadata.url===browser.url);
  await browser.cdp.eval("document.querySelector('input').value='main only';true");
  await browser.cdp.call('Page.bringToFront');
  const fromFrame=await tool('browser_script',{source,inputs:{url},resource_id:frame.resource_id});
  assert.equal(fromFrame.result,'approved');
  assert.equal(await browser.cdp.eval("document.querySelector('input').value"),'main only');
  assert.equal(await browser.cdp.eval("document.querySelector('iframe').contentDocument.querySelector('input').value"),'approved');
  for (const [name,code] of [['Ambiguous','BROWSER.PAGE_AMBIGUOUS'],['No opener','CONTROL.TIMEOUT'],['From frame','CONTROL.TIMEOUT']]) {
    await browser.cdp.call('Page.bringToFront');
    const refused=await tool('browser_script',{url:browser.url,timeout_ms:3500,
      source:`page.openPopup({name:inputs.name},inputs.url); page.click({role:'button',name:'Allow'});`,inputs:{url,name}},true);
    assert.equal(refused.error.code,code,name+': '+JSON.stringify(refused));
    assert.equal(refused.completed_actions,1);
  }
  await browser.cdp.call('Page.bringToFront');
  const waiting=tool('browser_script',{url:browser.url,source:"page.openPopup({name:'No popup'});page.click({role:'button',name:'Allow'})"},true);
  for(let tries=0; tries<100 && !await browser.cdp.eval('!!window.waiting'); tries++) await delay(25);
  assert.equal(await browser.cdp.eval('!!window.waiting'),true);
  tool.cancel();
  const cancelled=await waiting;
  assert.equal(cancelled.error.code,'CONTROL.CANCELLED');
  assert.equal(cancelled.completed_actions,1);
  const limited=mcp(['--resource',main.resource_id]);
  const refused=await limited('browser_script',{resource_id:main.resource_id,source,inputs:{url}},true);
  assert.equal(refused.error.code,'POLICY.DENIED');
  assert.equal(refused.completed_actions,0);
});
