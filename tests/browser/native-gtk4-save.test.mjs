import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {mkdtemp, readFile, rm, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {test} from 'node:test';
import {setTimeout as delay} from 'node:timers/promises';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

const exec=promisify(execFile);
async function waitFor(check) {
  for(let i=0;i<200;i++){if(await check())return;await delay(50);}
  assert.fail('timed out waiting for the GTK4 chooser or page result');
}

test('a browser-owned GTK4 Save dialog writes the authorized new file on X11', {
  skip:process.env.AAD_TEST_DIRECT_GTK4_SAVE!=='1' || !process.env.AAD_TEST_CHROME, timeout:120_000
},async t=>{
  const root=await mkdtemp(join(tmpdir(),'aad-gtk4-'));
  t.after(()=>rm(root,{recursive:true,force:true}));
  const page=await fixture((_req,res)=>res.end('GTK4 native Save'),['--ozone-platform=x11','--gtk-version=4'],{headed:true});
  t.after(()=>page.close());
  const active=async()=>(await exec('xdotool',['getactivewindow'])).stdout.trim();
  const owner=await active();
  await page.cdp.call('Runtime.evaluate',{userGesture:true,expression:`void showSaveFilePicker({suggestedName:'suggested.txt'})
    .then(async handle=>{const out=await handle.createWritable();await out.write('GTK4 page bytes');await out.close();window.saved=handle.name;})
    .catch(e=>window.err=String(e))`});
  await waitFor(async()=>(await active())!==owner);
  const dialog=await active();
  const dest=join(root,'new 文 件.txt'),existing=join(root,'existing.txt');
  await writeFile(existing,'original');
  const save=file=>exec(process.env.AAD_TEST_CLI,['--compact','browser','save-as','--cdp-port',String(page.port),
    '--page',page.target.id,'--allow-input','--allow-save-dir',root,'--file',file],{
    env:{...process.env,AAD_CONTROL_LEASE_DIR:join(root,'leases')}});
  await assert.rejects(save(existing));
  assert.equal(await active(),dialog);
  assert.equal(await readFile(existing,'utf8'),'original');
  const result=JSON.parse((await save(dest)).stdout);
  assert.equal(result.window,Number(dialog));
  assert.equal(result.destination_selected,true);
  assert.equal(result.picker_closed,true);
  assert.equal(result.path,dest);
  await waitFor(()=>page.cdp.eval('!!(window.saved||window.err)'));
  assert.deepEqual(await page.cdp.eval('[window.saved,window.err]'),['new 文 件.txt',null]);
  assert.equal(await readFile(dest,'utf8'),'GTK4 page bytes');
  assert.equal(await active(),owner);
});
