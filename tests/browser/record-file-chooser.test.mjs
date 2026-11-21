import assert from 'node:assert/strict';
import {execFile, spawn} from 'node:child_process';
import {once} from 'node:events';
import {mkdtemp,readFile,rm,writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join, resolve} from 'node:path';
import {test} from 'node:test';
import {setTimeout as delay} from 'node:timers/promises';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

const exec = promisify(execFile);
async function until(probe, description) {
  for (let tries = 0; tries < 120; tries++) {
    if (await probe().catch(() => false)) return;
    await delay(60);
  }
  throw Error(`Timed out: ${description}`);
}

test('headed file picker: associated labels, custom clicks, cancel and missing outcomes', {
  skip: !process.env.AAD_TEST_CHROME || !process.env.AAD_TEST_PICKER_X11, timeout: 90000
}, async t => {
  const page = await fixture((req, res) => {
    res.setHeader('Content-Type', 'text/html');
    res.end(`<title>Attachment fixture</title><label for="field"><span>Attachments</span></label>
      <input type="file" id="field" hidden><output></output>
      <button id="known" onclick="field.click()">Browse</button>
      <button id="detached" onclick="const temp=document.createElement('input');temp.type='file';temp.click()">Other</button>
      <button id="async" onclick="setTimeout(()=>field.click(),0)">Browse later</button>
      <button id="handles" onclick="showOpenFilePicker().catch(()=>window.handlesClosed=true)">File handles</button>
      <script>window.cancels=0;field.oncancel=()=>window.cancels++;
      field.onchange=async()=>{const f=field.files[0];field.setAttribute('aria-label',f.name);document.querySelector('output').textContent=f.name+await f.text();fetch('/posted',{method:'POST',body:await f.text()})};</script>`);
  }, ['--ozone-platform=x11','--gtk-version=3','--lang=en-US'], {headed:true});
  t.after(() => page.close());
  const cli = process.env.AAD_TEST_CLI || resolve('target/debug/aad');
  const out = join(page.root, 'recording');
  const run = async (...args) => JSON.parse((await exec(cli, ['browser-record', ...args], {
    env:{...process.env, AAD_RECORDINGS_DIR:join(page.root, 'store')}
  })).stdout);
  const recorder = spawn(cli, ['browser-record','start','--page',page.target.id,'--cdp-port',String(page.port),'--out',out], {stdio:'ignore'});
  t.after(() => recorder.kill());
  const stopped = once(recorder, 'exit');
  const evidence = () => readFile(join(out,'evidence','actions.jsonl'), 'utf8');
  await until(async () => JSON.parse(await readFile(join(out,'manifest.json'))).status === 'recording', 'recorder');
  const xd = (...args) => exec('xdotool', args);
  const dialogTitle = async () => (await xd('getactivewindow','getwindowname')).stdout.trim();
  const dialog = async () => /open|choose|upload|select a file|打开|開く|选择文件/i.test(await dialogTitle());
  const open = async selector => {
    const point = await page.cdp.eval(`(()=>{const el=document.querySelector(${JSON.stringify(selector)});if(!el||!el.getClientRects().length)throw Error('no visible fixture trigger');const r=el.getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2}})()`);
    for (const type of ['mousePressed','mouseReleased']) await page.cdp.call('Input.dispatchMouseEvent',{type,button:'left',clickCount:1,...point});
    try { await until(dialog, 'native chooser'); }
    catch (err) { throw Error(`${err.message}: ${selector}: ${await dialogTitle()}`); }
  };
  assert.equal(await page.cdp.eval(`(()=>{const el=document.querySelector('#known');if(el?.textContent!=='Browse')return false;el.focus();return document.activeElement===el})()`), true);
  await xd('key','Return');
  await until(dialog, 'keyboard opens native chooser');
  await xd('key','Escape');
  await until(() => page.cdp.eval('window.cancels===1'), 'keyboard chooser cancel event');
  await open('#async');
  await xd('key','Escape');
  await until(() => page.cdp.eval('window.cancels===2'), 'async chooser cancel event');
  await open('#handles');
  await xd('key','Escape');
  await until(() => page.cdp.eval('window.handlesClosed'), 'file handle chooser closed');
  await open('#detached');
  await xd('key','Escape');
  await until(async () => !(await dialog()), 'detached chooser closed');
  const filename = 'original-private-239.txt', contents = 'private-payload-239';
  const files = await mkdtemp(join(tmpdir(),'aad-picker-files-'));
  t.after(() => rm(files,{recursive:true,force:true}));
  const path = join(files,filename);
  await writeFile(path,contents);
  await open('label span');
  await delay(300); await xd('key','--clearmodifiers','ctrl+l'); await delay(250);
  await xd('type','--clearmodifiers','--delay','10','--',path); await delay(300);
  const window = (await xd('getactivewindow')).stdout.trim();
  const geometry = (await xd('getwindowgeometry','--shell',window)).stdout;
  const width = Number(geometry.match(/^WIDTH=(\d+)$/m)[1]), height = Number(geometry.match(/^HEIGHT=(\d+)$/m)[1]);
  assert.ok(await dialog() && (await xd('getactivewindow')).stdout.trim() === window);
  await xd('mousemove','--window',window,String(width-50),String(height-25),'click','1');
  try { await until(async () => await page.cdp.eval('document.querySelector("output").textContent') === filename+contents, 'actual file bytes'); }
  catch (err) { throw Error(`${err.message}: ${await dialogTitle()}: ${(await evidence()).trim().split('\n').map(row=>JSON.parse(row).action.kind).join(',')}`); }
  await until(async () => (await evidence()).includes('"kind":"upload"'), 'selection checkpoint');
  await open('label span');
  await xd('key','Escape');
  await until(() => page.cdp.eval('window.cancels===3'), 'cancel on already-selected file');
  await page.cdp.eval("fetch('/after-upload').then(r=>r.text())");
  await run('stop','--out',out);
  assert.equal((await stopped)[0], 0);
  const exported = await run('steps','--out',out);
  assert.deepEqual(exported.steps.map(step=>step.action), [{type:'upload'}], JSON.stringify(exported));
  assert.deepEqual(exported.steps[0].input, {name:'rec1_files',sensitive:false});
  assert.deepEqual(exported.steps[0].locator, {role:'button',name:'Attachments',states:{hidden:true}});
  const reasons = exported.skipped.map(item=>item.reason);
  assert.deepEqual(reasons.sort(), [...Array(3).fill('custom_file_chooser_trigger_unresolved'),'file_chooser_outcome_unobserved','file_chooser_without_dom_input',...Array(3).fill('file_selection_cancelled_or_unchanged')]);
  assert.ok(exported.skipped.filter(item=>item.reason==='file_selection_cancelled_or_unchanged').every(item=>item.noop===true));
  const rawActions = await evidence();
  for (const data of [rawActions, JSON.stringify(exported)]) assert.ok(!data.includes(filename) && !data.includes(contents));
  assert.ok((await readFile(join(out,'evidence','network.jsonl'),'utf8')).includes('/after-upload'));
});
