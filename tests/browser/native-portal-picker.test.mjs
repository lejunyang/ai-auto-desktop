import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {mkdir, mkdtemp, readFile, rm, symlink, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join, resolve} from 'node:path';
import {test} from 'node:test';
import {setTimeout as delay} from 'node:timers/promises';
import {promisify} from 'node:util';
import {clickPickerButton, fixture} from './fixture.mjs';
import {verifyDirectory} from './portal-directory.mjs';

const exec = promisify(execFile);
const desktop = process.env.XDG_SESSION_TYPE;
const portal = 'org.freedesktop.impl.portal.desktop.gtk';
async function requests() {
  const {stdout} = await exec('busctl', ['--address='+process.env.DBUS_SESSION_BUS_ADDRESS,
    '--list','tree',portal]);
  return stdout.trim().split('\n').filter(s=>/\/request\/[^/]+\/[^/]+$/.test(s));
}
async function waitFor(check) {
  for(let i=0;i<200;i++){if(await check())return;await delay(50);}
  assert.fail('timed out waiting for the native chooser or its page result');
}

test(`an already-open GTK portal supports Open and new-file Save on ${desktop}`, {
  skip:process.env.AAD_TEST_FILE_PORTAL!=='1' || !process.env.AAD_TEST_CHROME,
  timeout:120_000
}, async t=>{
  assert.ok(['wayland','x11'].includes(desktop));
  const root = await mkdtemp(join(tmpdir(),'aad-picker-grant-'));
  t.after(()=>rm(root,{recursive:true,force:true}));
  const page = await fixture((req,res)=>{
    res.setHeader('Content-Type','text/html;charset=utf-8');
    if (req.url==='/other-window') {
      const port=new URL('http://'+req.headers.host).port;
      res.end('<iframe src="http://localhost:'+port+'/foreign-elsewhere"></iframe>');
      return;
    }
    res.end('<input id=f type=file><input id=other type=file><input id=s value=untouched>');
  }, ['--ozone-platform='+desktop,'--site-per-process'], {headed:true});
  t.after(()=>page.close());
  const source = join(root,'original.txt'), dest = join(root,'new 文 件.txt'), dangling = join(root,'dangling');
  await writeFile(source,'source bytes');
  await symlink(join(root,'missing'),dangling);
  const cli=process.env.AAD_TEST_CLI || resolve('target/debug/aad');
  const run=(command,file,{grant=root,native=false}={})=>exec(cli,['--compact','browser',command,'--page',page.target.id,
    '--cdp-port',String(page.port),'--allow-input',command==='save-as'?'--allow-save-dir':'--allow-upload-dir',
    grant,'--file',file,...(native?['--native-file']:[])],{env:{...process.env,AAD_CONTROL_LEASE_DIR:join(root,'leases')}});
  const gesture=expression=>page.cdp.call('Runtime.evaluate',{expression,userGesture:true});
  await gesture('f.oncancel=()=>window.cancelled=true; f.click()');
  await waitFor(async()=>(await requests()).length===1 && await page.cdp.eval('f.matches(":open")'));
  const open=await requests();
  await assert.rejects(run('save-as',dest));
  assert.deepEqual(await requests(),open);
  if (desktop==='wayland') {
    const {targetId}=await page.cdp.call('Target.createTarget',{url:'about:blank',background:true});
    try {
      await assert.rejects(run('choose-files',source));
      assert.deepEqual(await requests(),open);
      assert.equal(await page.cdp.eval('f.files.length'),0);
    } finally {
      await page.cdp.call('Target.closeTarget',{targetId});
    }
  }
  const selected=JSON.parse((await run('choose-files',source)).stdout);
  assert.equal(selected.picker_closed,true);
  assert.deepEqual(await requests(),[]);
  assert.deepEqual(await page.cdp.eval('(async()=>[await f.files[0].text(),other.files.length,s.value,!!window.cancelled])()'),
    ['source bytes',0,'untouched',false]);

  const nativeSource=join(root,'native 文 件.txt');
  await writeFile(nativeSource,'bytes read through the native handle');
  if (desktop==='x11') {
    await page.cdp.call('Target.createTarget',{url:page.url+'other-window',newWindow:true,
      left:90,top:80,width:430,height:310});
    await waitFor(async()=>(await page.cdp.call('Target.getTargets')).targetInfos.some(target=>
      target.type==='iframe' && target.url.endsWith('/foreign-elsewhere')));
  }
  await clickPickerButton(page,'open-native',`void showOpenFilePicker({multiple:true}).then(async handles=>{
    window.handles=await Promise.all(handles.map(async h=>[h.name,await (await h.getFile()).text()]));
  }).catch(e=>window.openError=String(e))`);
  await waitFor(async()=>{
    const error=await page.cdp.eval('window.openError ?? null');
    assert.equal(error,null);
    return (await requests()).length===1;
  });
  const nativeRequest=await requests();
  assert.equal(await page.cdp.eval('document.querySelector("input:open")===null'),true);
  const denied=join(root,'empty-grant');
  await mkdir(denied);
  await assert.rejects(run('choose-files',nativeSource,{native:true,grant:denied}));
  assert.deepEqual(await requests(),nativeRequest);
  const {targetId}=await page.cdp.call('Target.createTarget',{url:'about:blank',background:true});
  try {
    await assert.rejects(run('choose-files',nativeSource,{native:true}));
    assert.deepEqual(await requests(),nativeRequest);
    assert.equal(await page.cdp.eval('window.handles===undefined'),true);
  } finally {
    await page.cdp.call('Target.closeTarget',{targetId});
  }
  await page.cdp.eval(`new Promise(resolve=>{
    const frame=document.createElement('iframe'); frame.id='peer-frame';
    frame.onload=()=>resolve(true); frame.srcdoc='<p>Another document</p>';
    document.body.appendChild(frame);
  })`);
  await assert.rejects(run('choose-files',nativeSource,{native:true}), err =>
    JSON.parse(err.stdout).error.message.includes('frame'));
  assert.deepEqual(await requests(),nativeRequest);
  const foreign=page.url.replace('127.0.0.1','localhost')+'child';
  await page.cdp.eval(`new Promise(resolve=>{
    const frame=document.getElementById('peer-frame');
    frame.onload=()=>resolve(true); frame.removeAttribute('srcdoc'); frame.src=${JSON.stringify(foreign)};
  })`);
  await waitFor(async()=>(await page.cdp.call('Target.getTargets')).targetInfos
    .some(target=>target.type==='iframe' && target.url===foreign));
  await page.cdp.eval("Object.defineProperty(window,'length',{value:0,configurable:true});true");
  await assert.rejects(run('choose-files',nativeSource,{native:true}), err =>
    JSON.parse(err.stdout).error.message.includes('frame'));
  assert.deepEqual(await requests(),nativeRequest);
  await page.cdp.eval('document.getElementById("peer-frame").remove()');
  const native=JSON.parse((await run('choose-files',nativeSource,{native:true})).stdout);
  assert.equal(native.native_file_selected,true);
  assert.equal(native.file_count,1);
  assert.equal(native.picker_closed,true);
  assert.deepEqual(await requests(),[]);
  await waitFor(()=>page.cdp.eval('!!(window.handles || window.openError)'));
  assert.deepEqual(await page.cdp.eval('[window.handles,window.openError,s.value]'),
    [[['native 文 件.txt','bytes read through the native handle']],null,'untouched']);

  // Native Save is bound to the page document and dialog. Its success should
  // be independent of whether thousands of unrelated controls fit in a snapshot.
  await page.cdp.eval(`for(let i=0;i<2100;i++){
    const b=document.createElement('button'); b.textContent='row '+i; document.body.appendChild(b);
  }`);
  await clickPickerButton(page,'save-native',`void showSaveFilePicker({suggestedName:'proposed.txt'}).then(async handle=>{
    window.chosen=handle.name;
    const output=await handle.createWritable(); await output.write('bytes written by the page'); await output.close();
    window.saved=true;
  }).catch(e=>window.saveError=String(e))`);
  await waitFor(async()=>(await requests()).length===1);
  const save=await requests();
  await assert.rejects(run('choose-files',source));
  await assert.rejects(run('save-as',source));
  await assert.rejects(run('save-as',dangling));
  assert.deepEqual(await requests(),save);
  assert.equal(await readFile(source,'utf8'),'source bytes');
  const result=JSON.parse((await run('save-as',dest)).stdout);
  assert.equal(result.destination_selected,true);
  assert.equal(result.picker_closed,true);
  assert.equal(result.path,dest);
  assert.deepEqual(await requests(),[]);
  await waitFor(()=>page.cdp.eval('!!(window.saved || window.saveError)'));
  assert.deepEqual(await page.cdp.eval('[window.chosen,window.saved,window.saveError,s.value]'),
    ['new 文 件.txt',true,null,'untouched']);
  assert.equal(await readFile(dest,'utf8'),'bytes written by the page');

  await clickPickerButton(page,'select-folder','void showDirectoryPicker().catch(()=>{})');
  await waitFor(async()=>(await requests()).length===1);
  const folderRequest=await requests();
  await assert.rejects(run('choose-files',nativeSource,{native:true}));
  assert.deepEqual(await requests(),folderRequest);
  await verifyDirectory(page, root, folderRequest[0], t);
  assert.deepEqual(await requests(),[]);
});
