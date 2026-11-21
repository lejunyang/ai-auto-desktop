import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {createHash} from 'node:crypto';
import {once} from 'node:events';
import {createServer} from 'node:net';
import {join, resolve} from 'node:path';
import {setTimeout as delay} from 'node:timers/promises';
import {test} from 'node:test';
import {fixture} from './fixture.mjs';

// Run in a private X11/DBus session with a custom-protocol build of aad-gui.
test('repair a recorded button in the desktop editor and replay just its suffix', {
  skip: !(process.env.AAD_TEST_WEBKIT_DRIVER && process.env.AAD_TEST_CHROME), timeout: 100_000,
}, async t => {
  const page = await fixture((_req, res) => {
    res.setHeader('Content-Type', 'text/html;charset=utf-8');
    res.end('<title>Expense fixture</title><button id="start">Start</button><button id="save">Save</button><script>window.hits=[];for(const b of document.querySelectorAll("button"))b.onclick=()=>hits.push(b.textContent)</script>');
  });
  const oldPng = (await page.cdp.call('Page.captureScreenshot', {format: 'png'})).data;
  const pngName = 'images/' + createHash('sha256').update(Buffer.from(oldPng, 'base64')).digest('hex') + '.png';
  const rows = ['Start','Save'].map((name,i) => JSON.stringify({
    id: i + 4, kind: 'action', action: {kind: 'click', control: {role:'button',name}, url: page.url},
    screenshots: i ? [{path:pngName}] : [],
  })).join('\n');
  await page.cdp.eval("save.textContent='Save revised';true");
  const socket = createServer().listen(0,'127.0.0.1');
  await once(socket,'listening');
  const port = socket.address().port;
  await new Promise(done=>socket.close(done));
  const driver = spawn(process.env.AAD_TEST_WEBKIT_DRIVER, ['--port='+port], {detached:true,env: {
    ...process.env, TAURI_WEBVIEW_AUTOMATION:'true', AAD_RECORDINGS_DIR:join(page.root,'recordings'),
  }});
  let logs = '';
  driver.stderr.on('data', chunk=>logs+=chunk);
  driver.stdout.on('data', chunk=>logs+=chunk);
  let sessionPath;
  t.after(async ()=>{
    if(sessionPath) await wd(sessionPath,null,'DELETE').catch(()=>{});
    try { process.kill(-driver.pid,'SIGKILL'); } catch {}
    await page.close();
  });
  async function wd(path, args, method = args ? 'POST' : 'GET') {
    const r = await fetch('http://127.0.0.1:'+port+path, {method,headers:{'Content-Type':'application/json'},body:args && JSON.stringify(args),signal:AbortSignal.timeout(path === '/session' ? 55000 : 10000)})
      .catch(error=>{throw Error(path+' '+error+' '+logs.slice(-2500))});
    const {value} = await r.json();
    if (!r.ok) throw Error(JSON.stringify(value));
    return value;
  }
  async function until(fn,label) {
    for (let i=0;i<150;i++) { if (await fn()) return; await delay(100); }
    const pageText = sessionPath && await wd(sessionPath+'/execute/sync',{script:'return document.body.innerText.slice(-1500)',args:[]}).catch(()=>null);
    throw Error('Timed out waiting for '+label+' '+pageText+' '+logs.slice(-1500));
  }
  await until(()=>wd('/status').catch(()=>false),'WebKitWebDriver');
  const {sessionId} = await wd('/session',{capabilities:{alwaysMatch:{'webkitgtk:browserOptions':{
    binary: process.env.AAD_TEST_GUI || resolve('target/debug/aad-gui'),
  }}}});
  const base='/session/'+sessionId;
  sessionPath=base;
  const js=(script,...args)=>wd(base+'/execute/sync',{script,args});
  const click=(label,selector='button')=>until(()=>js(
    'const b=[...document.querySelectorAll(arguments[1])].find(x=>x.textContent.includes(arguments[0])&&!x.disabled);if(b)b.click();return !!b',label,selector),label);
  await click('打开…');
  await js(`const [rows,png,name]=arguments;
    const data=Uint8Array.from(atob(png),c=>c.charCodeAt(0));
    const dt=new DataTransfer();
    for(const [path,parts] of [['steps.jsonl',[rows]],[name,[data]]]){
      const f=new File(parts,path.split('/').pop());
      Object.defineProperty(f,'webkitRelativePath',{value:'recording/'+path});dt.items.add(f);
    }
    const input=document.querySelector('input[type=file]');input.files=dt.files;
    input.dispatchEvent(new Event('change',{bubbles:true}));`,rows,oldPng,pngName);
  await until(()=>js('return document.querySelectorAll("article.card").length===2'),'recorded actions');
  await js('const i=document.querySelector("input[inputmode=numeric]");i.value=arguments[0];i.dispatchEvent(new Event("input",{bubbles:true}))',String(page.port));
  await click('连接','footer.browser button');
  await click('Expense fixture','button.target');
  await js('document.querySelectorAll("button[title=编辑]")[1].click()');
  await click('对照并替换元素');
  await until(()=>js(`const b=[...document.querySelectorAll('button.node')].find(e=>e.querySelector('.role')?.textContent==='button'&&e.textContent.includes('Save revised'));if(b)b.click();return !!b`),'replacement button');
  await click('检查所选元素');
  await until(()=>js('return [...document.querySelectorAll(".compare img")].filter(i=>i.naturalWidth>0).length===2'),'old screenshot and live element screenshot');
  await click('替换步骤目标');
  await until(()=>js('return document.querySelectorAll("article.card .title")[1].textContent.includes("Save revised")'),'replacement target');
  await js('document.querySelectorAll("button[title=编辑]")[1].click()');
  await click('从这一步试跑');
  await click('运行这 1 步');
  await until(()=>page.cdp.eval('hits.length>0'),'replayed action');
  assert.deepEqual(await page.cdp.eval('hits'),['Save revised']);
});
