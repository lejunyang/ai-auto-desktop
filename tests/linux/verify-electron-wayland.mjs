import assert from 'node:assert/strict';
import {execFile, spawn} from 'node:child_process';
import {mkdtemp, mkdir, readFile, rm, stat, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join, resolve} from 'node:path';
import {fileURLToPath} from 'node:url';
import {setTimeout as delay} from 'node:timers/promises';
import {promisify} from 'node:util';
import {connect} from '../browser/fixture.mjs';

const run=promisify(execFile);
const cli=resolve(process.env.AAD_TEST_CLI || 'target/debug/aad');
const code=process.env.AAD_TEST_VSCODE || '/usr/share/code/code';
const privateBus=process.argv[2]==='--private-bus';
const root=privateBus?process.env.XDG_RUNTIME_DIR:await mkdtemp(join(tmpdir(),'aad-code-wayland-'));
const env={...process.env, XDG_RUNTIME_DIR:root, XDG_SESSION_TYPE:'wayland', XDG_CURRENT_DESKTOP:'KDE', WAYLAND_DISPLAY:'aad-verify',
  XDG_CONFIG_HOME:join(root,'config'),XDG_CACHE_HOME:join(root,'cache'),XDG_DATA_HOME:join(root,'data'),
  AAD_CONTROL_LEASE_DIR:join(root,'leases')};
for(const key of ['DISPLAY','AT_SPI_BUS_ADDRESS','NO_AT_BRIDGE','WAYLAND_SOCKET','VSCODE_IPC_HOOK_CLI','ELECTRON_RUN_AS_NODE'])delete env[key];
if(!privateBus) {
  try {
    const child=spawn('dbus-run-session',['--',process.execPath,fileURLToPath(import.meta.url),'--private-bus'],{env,stdio:'inherit'});
    process.exitCode=await new Promise((res,rej)=>{child.once('error',rej);child.once('exit',code=>res(code??1));});
  } finally {await rm(root,{recursive:true,force:true,maxRetries:10,retryDelay:100});}
} else {
  await verify();
}

async function verify() {
  const children=[];
  function launch(bin,args) {
    const child=spawn(bin,args,{env,stdio:['ignore','ignore','inherit'],detached:true});
    children.push(child);
    return child;
  }
  async function until(get,description) {
    const deadline=Date.now()+35_000;
    while(Date.now()<deadline) {
      const result=await get();
      if(result)return result;
      await delay(150);
    }
    throw new Error('Timed out waiting for '+description);
  }
  let cdp;
  try {
    launch('kwin_wayland',['--virtual','--no-lockscreen','--socket',env.WAYLAND_DISPLAY]);
    await until(()=>stat(join(root,env.WAYLAND_DISPLAY)).then(s=>s.isSocket(),()=>false),'Wayland compositor');
    const profile=join(root,'profile'),user=join(profile,'User');
    await mkdir(user,{recursive:true});
    await writeFile(join(user,'settings.json'),JSON.stringify({'workbench.startupEditor':'none','telemetry.telemetryLevel':'off',
      'update.mode':'none','window.titleBarStyle':'custom','workbench.tips.enabled':false,'editor.accessibilitySupport':'on'}));
    const file=join(root,'invoice.txt');
    await writeFile(file,'Invoice 2037\n');
    const codeProcess=launch(code,['--ozone-platform=wayland','--remote-debugging-port=0','--remote-debugging-address=127.0.0.1',
      '--user-data-dir='+profile,'--shared-data-dir='+join(root,'shared'),'--use-inmemory-secretstorage','--extensions-dir='+join(root,'extensions'),'--password-store=basic','--disable-workspace-trust','--disable-gpu','--locale=en','--disable-extensions','--disable-updates','--skip-welcome','--new-window',file]);
    const port=await until(async()=>{
      if(codeProcess.exitCode!==null)throw new Error('VS Code exited '+codeProcess.exitCode);
      return readFile(join(profile,'DevToolsActivePort'),'utf8').then(data=>Number(data.split('\n')[0]),()=>0);
    },'VS Code debugging port');
    const target=await until(async()=>{
      const tabs=await (await fetch('http://127.0.0.1:'+port+'/json/list',{signal:AbortSignal.timeout(4000)})).json();
      return tabs.find(t=>t.type==='page' && t.url.startsWith('vscode-file://vscode-app/') && t.url.includes('/workbench.html'));
    },'VS Code workbench');
    cdp=await connect(target.webSocketDebuggerUrl);
    const searchTab=await until(()=>cdp.eval(`document.querySelector('.activitybar [role=tab] [aria-label^=Search]')?.ariaLabel`),'Search tab');
    const flags=['--browser-port',String(port),'--url',target.url];
    const script=join(root,'search.js');
    const execute=async(source,inputs={})=>{
      await writeFile(script,source);
      return run(cli,['browser','script',script,...flags,'--allow-app-document',target.url,'--allow-input','--inputs',JSON.stringify(inputs)],{env});
    };
    const observe=async()=>JSON.parse((await run(cli,['browser','read',...flags,'--allow-app-document',target.url,'--view','nodes'],{env,maxBuffer:8*1024*1024})).stdout);
    const matching=(await observe()).nodes.filter(n=>n.name===searchTab);
    const tab=matching.find(n=>n.role==='tab') ?? matching.find(n=>n.role==='link');
    assert.ok(tab,'expected Search tab in the observation');
    await assert.rejects(run(cli,['browser','read',...flags],{env}));
    await cdp.eval(`window.__aadTestInput=[];for(const type of ['mousedown','mouseup','keydown'])document.addEventListener(type,
      e=>window.__aadTestInput.push([type,e.isTrusted,e.key??'',e.target.closest('[role=tab]')?.querySelector('[aria-label]')?.ariaLabel??'']),true);true`);
    await execute('return page.act(inputs.tab,{type:"click",fraction:{fx:.5,fy:.5}})',{tab:{role:tab.role,name:tab.name}});
    const field=await until(()=>cdp.eval(`[...document.querySelectorAll('.search-view textarea')].find(el=>el.getBoundingClientRect().height>0)?.ariaLabel`),'Search field');
    assert.match(field,/Search/);
    const loc={role:'textbox',name:'Search',match:'contains',states:{focused:true}};
    await execute('return page.fill(inputs.loc,"Invoice-2037")',{loc});
    assert.equal(await cdp.eval('document.activeElement.value'),'Invoice-2037');
    await execute('return page.key(inputs.loc,["ctrl","a"])',{loc});
    const state=await cdp.eval(`(()=>{const el=document.activeElement;return [el.value,el.selectionStart,el.selectionEnd]})()`);
    assert.deepEqual(state,['Invoice-2037',0,'Invoice-2037'.length]);
    await execute('return page.key(inputs.loc,["Backspace"])',{loc});
    assert.equal(await cdp.eval('document.activeElement.value'),'');
    const events=await cdp.eval('window.__aadTestInput');
    assert.ok(events.some(([type,trusted,,tab])=>type==='mousedown' && trusted && tab===searchTab));
    assert.ok(events.some(([type,trusted,key])=>type==='keydown' && trusted && key==='Backspace'));
    console.log(JSON.stringify({tab:searchTab,field,selection:state,afterDeletion:'',wayland:env.WAYLAND_DISPLAY}));
  } finally {
    cdp?.close();
    for(const child of children.reverse()) {
      try{process.kill(-child.pid,'SIGTERM');}catch{}
    }
    await delay(350);
    for(const child of children){try{process.kill(-child.pid,'SIGKILL');}catch{}}
  }
}

