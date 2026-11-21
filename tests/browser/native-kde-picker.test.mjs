import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {mkdir,mkdtemp,readFile,rm,writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {test} from 'node:test';
import {setTimeout as delay} from 'node:timers/promises';
import {promisify} from 'node:util';
import {clickPickerButton, fixture} from './fixture.mjs';
import {verifyDirectory} from './portal-directory.mjs';

const exec=promisify(execFile);
const kde='org.freedesktop.impl.portal.desktop.kde',front='org.freedesktop.portal.Desktop';
const bus=async(...args)=>(await exec('busctl',['--address='+process.env.DBUS_SESSION_BUS_ADDRESS,...args])).stdout;
const dbus=async(method,service)=>JSON.parse(await bus('--json=short','call','org.freedesktop.DBus',
  '/org/freedesktop/DBus','org.freedesktop.DBus',method,'s',service)).data[0];
async function requests() {
  return (await bus('--list','tree',front)).split('\n').filter(s=>/\/request\/[^/]+\/[^/]+$/.test(s));
}
async function until(check) {
  for(let i=0;i<300;i++) {const value=await check();if(value)return value;await delay(50);}
  assert.fail('timed out waiting for the KDE request or Chromium result');
}
async function observedRequest(t) {
  const handle=await until(async()=>{const active=await requests();return active.length===1 && active[0];});
  const owner=await dbus('GetNameOwner',kde),pid=await dbus('GetConnectionUnixProcessID',owner);
  const parent=handle.slice(0,handle.lastIndexOf('/')),token=handle.slice(parent.length+1);
  await until(async()=>{
    try {
      const xml=JSON.parse(await bus('--json=short','call',owner,parent,'org.freedesktop.DBus.Introspectable','Introspect')).data[0];
      return xml.includes('<node name="'+token+'"/>');
    } catch(e) {
      if (!e.stderr?.includes('No such object path')) throw e;
      return false;
    }
  });
  const caller=':'+parent.split('/').at(-1).replace('_','.');
  const browser=await dbus('GetConnectionUnixProcessID',caller);
  const {stdout}=await exec('/usr/bin/python3',['tests/linux/kde-picker-fixture.py',String(pid)]);
  t.diagnostic(`${kde} owned by ${owner} (PID ${pid}), dialog ${stdout.trim()}, request ${handle}, caller PID ${browser}`);
  return {handle,browser};
}

test('Chromium reads and saves files through an already-open KDE Wayland portal', {
  skip:process.env.AAD_TEST_KDE_PICKER!=='1',timeout:120_000
},async t=>{
  const root=await mkdtemp(join(tmpdir(),'kde-grant-'));
  t.after(()=>rm(root,{recursive:true,force:true}));
  const other=await fixture((_q,res)=>res.end('other browser window'),['--ozone-platform=wayland'],{headed:true});
  t.after(()=>other.close());
  const p=await fixture((_q,res)=>{res.setHeader('Content-Type','text/html;charset=utf-8');res.end('<input type=file id=f><input id=s value=untouched>');},
    ['--ozone-platform=wayland'],{headed:true});
  t.after(()=>p.close());
  const source=join(root,'source 文 件.txt'), destination=join(root,'new 文 件.txt'), denied=join(root,'empty');
  await mkdir(denied);await writeFile(source,'KDE source bytes');
  const run=(command,file,{grant=root,browser=p,native=false}={})=>exec(process.env.AAD_TEST_CLI,[
    '--compact','browser',command,'--cdp-port',String(browser.port),'--page',browser.target.id,'--allow-input',
    command==='save-as'?'--allow-save-dir':'--allow-upload-dir',grant,'--file',file,...(native?['--native-file']:[])
  ],{env:{...process.env,AAD_CONTROL_LEASE_DIR:join(root,'leases')}});
  const gesture=expression=>p.cdp.call('Runtime.evaluate',{expression,userGesture:true});
  await p.cdp.call('Page.bringToFront');
  await gesture('f.oncancel=()=>window.cancelled=true;f.click()');
  const open=await observedRequest(t);
  assert.equal(await p.cdp.eval('f.matches(":open")'),true);
  await assert.rejects(run('save-as',destination));
  await assert.rejects(run('choose-files',source,{grant:denied}));
  await assert.rejects(run('choose-files',source,{browser:other}));
  assert.deepEqual(await requests(),[open.handle]);
  assert.equal(await p.cdp.eval('f.files.length'),0);
  const selected=JSON.parse((await run('choose-files',source)).stdout);
  assert.equal(selected.picker_closed,true);assert.equal(selected.browser_pid,open.browser);
  assert.deepEqual(await requests(),[]);
  assert.deepEqual(await p.cdp.eval('(async()=>[await f.files[0].text(),!!window.cancelled,s.value])()'),['KDE source bytes',false,'untouched']);
  await clickPickerButton(p,'save-native',`void showSaveFilePicker({suggestedName:'suggested.txt'}).then(async h=>{
    const out=await h.createWritable();await out.write('saved from Chromium');await out.close();window.saved=h.name;
  }).catch(e=>window.error=String(e))`);
  const save=await observedRequest(t);
  await assert.rejects(run('choose-files',source));
  await assert.rejects(run('save-as',source));
  await assert.rejects(run('save-as',destination,{grant:denied}));
  assert.deepEqual(await requests(),[save.handle]);
  const result=JSON.parse((await run('save-as',destination)).stdout);
  assert.equal(result.path,destination);assert.equal(result.destination_selected,true);assert.equal(result.picker_closed,true);
  await until(()=>p.cdp.eval('window.saved||window.error'));
  assert.equal(await p.cdp.eval('window.saved'),'new 文 件.txt');
  assert.equal(await readFile(destination,'utf8'),'saved from Chromium');
  assert.equal(await readFile(source,'utf8'),'KDE source bytes');
  assert.deepEqual(await requests(),[]);
  await clickPickerButton(p,'open-native',`void showOpenFilePicker({multiple:true}).then(async hs=>{
    window.handles=await Promise.all(hs.map(async h=>[h.name,await (await h.getFile()).text()]));
  }).catch(e=>window.error=String(e))`);
  await observedRequest(t);
  const native=JSON.parse((await run('choose-files',source,{native:true})).stdout);
  assert.equal(native.native_file_selected,true);assert.equal(native.file_count,1);assert.equal(native.picker_closed,true);
  await until(()=>p.cdp.eval('window.handles||window.error'));
  assert.deepEqual(await p.cdp.eval('window.handles'),[['source 文 件.txt','KDE source bytes']]);
  assert.deepEqual(await requests(),[]);
  await clickPickerButton(p,'directory',`void showDirectoryPicker().then(h=>window.directory=h.name).catch(e=>window.error=String(e))`);
  const folder=await observedRequest(t);
  await assert.rejects(run('choose-files',source,{native:true}));
  await verifyDirectory(p, root, folder.handle, t);
  assert.deepEqual(await requests(),[]);
});
