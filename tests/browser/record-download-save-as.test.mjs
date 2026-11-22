import assert from 'node:assert/strict';
import {execFile, spawn} from 'node:child_process';
import {once} from 'node:events';
import {mkdtemp, readFile, readdir, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {test} from 'node:test';
import {setTimeout as delay} from 'node:timers/promises';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

const exec = promisify(execFile);
const bytes = Buffer.from([7, 0, 255, 42, 17, 13, 10]);
async function until(check, description) {
  for (let i = 0; i < 240; i++) {
    const value = await check();
    if (value) return value;
    await delay(50);
  }
  assert.fail('Timed out waiting for ' + description);
}

for (const mode of ['save', 'cancel', 'ambiguous', 'automatic']) {
  const headed = mode !== 'automatic';
  test('record download link with ' + mode, {
    skip: !process.env.AAD_TEST_CHROME || (headed && process.env.AAD_TEST_DIRECT_GTK4_SAVE !== '1'),
    timeout: 90000
  }, async t => {
    let payload = bytes, requests = 0;
    const page = await fixture((req, res) => {
      if (req.url === '/payload') {
        requests++;
        return res.writeHead(200, {'Content-Type':'application/octet-stream'}).end(payload);
      }
      res.writeHead(200, {'Content-Type':'text/html'}).end(
        '<a href="/payload" download="report.bin">Get report</a>');
    }, headed ? ['--ozone-platform=x11','--gtk-version=4'] : [], {headed, downloadPrompt:headed});
    t.after(() => page.close());
    const out = join(page.root, 'capture');
    const grant = await mkdtemp(join(tmpdir(), 'aad-download-grant-'));
    t.after(() => rm(grant, {recursive:true, force:true}));
    const env = {...process.env, AAD_CONTROL_LEASE_DIR:join(page.root,'leases'),
      AAD_RECORDINGS_DIR:join(page.root,'store'), AAD_CDP_PORT:String(page.port)};
    const run = async (...args) => {
      try { return JSON.parse((await exec(env.AAD_TEST_CLI,args,{env})).stdout); }
      catch (e) { throw Error(e.stdout || e.message); }
    };
    const record = spawn(env.AAD_TEST_CLI, ['browser-record','start','--page',page.target.id,
      '--cdp-port',String(page.port),'--out',out,'--no-screenshots'], {env,stdio:'ignore'});
    const exited = once(record,'exit');
    t.after(() => record.kill());
    await until(async () => {
      try { return JSON.parse(await readFile(join(out,'manifest.json'))).status === 'recording'; }
      catch { return false; }
    }, 'recorder startup');
    await page.cdp.call('Page.enable',{enableFileChooserOpenedEvent:true});
    const active = async () => (await exec('xdotool',['getactivewindow'])).stdout.trim();
    const owner = headed ? await active() : null;
    const click = async () => {
      const point = await page.cdp.eval(`(() => {
        const a=document.querySelector('a[download]'), r=a.getBoundingClientRect();
        const x=r.x+r.width/2, y=r.y+r.height/2;
        if (a.textContent !== 'Get report' || document.elementFromPoint(x,y) !== a) throw Error('link changed');
        return {x,y};
      })()`);
      for (const type of ['mousePressed','mouseReleased'])
        await page.cdp.call('Input.dispatchMouseEvent',{type,button:'left',clickCount:1,...point});
    };
    if (mode === 'ambiguous') {
      await page.cdp.eval("document.querySelector('a').addEventListener('click', e=>e.preventDefault(), {once:true})");
      await click();
    }
    await click();
    const begin = await until(() => page.cdp.events.find(e => e.method === 'Page.downloadWillBegin'), 'download GUID');
    assert.equal(begin.params.frameId,(await page.cdp.call('Page.getFrameTree')).frameTree.frame.id);
    const original = join(grant,'private-original.bin');
    if (headed) {
      const dialog = await until(async () => {const w=await active(); return w!==owner && w;}, 'native Save dialog');
      if (mode !== 'ambiguous') {
        await until(async () => (await readFile(join(out,'evidence/actions.jsonl'),'utf8')).split('\n')
          .filter(Boolean).some(line=>{const r=JSON.parse(line);return r.kind==='save_picker_pending' && r.action.source==='download';}),
        'read-only dialog observation');
      }
      if (mode === 'cancel') {
        assert.equal(await active(),dialog);
        await exec('xdotool',['key','Escape']);
      } else {
        const result=await run('browser','save-as','--page',page.target.id,'--cdp-port',String(page.port),
          '--allow-input','--allow-save-dir',grant,'--file',original);
        assert.equal(result.window,Number(dialog));
      }
    }
    await until(() => page.cdp.events.find(e=>e.method==='Page.downloadProgress'
      && e.params.guid===begin.params.guid && e.params.state===(mode==='cancel'?'canceled':'completed')), 'terminal state');
    await run('browser-record','stop','--out',out);
    assert.equal((await exited)[0],0);
    const result=await run('browser-record','steps','--out',out,...(mode==='save'?['--save','download-report']:[]));
    assert.equal(requests,1);
    if (mode==='automatic' || mode==='ambiguous') {
      assert.ok(result.steps.length);
      assert.ok(result.steps.every(s=>s.action.type==='click'));
      assert.deepEqual(await readFile(mode==='automatic'?join(page.root,'downloads','report.bin'):original),bytes);
      return;
    }
    if (mode==='cancel') {
      assert.deepEqual(result.steps,[]);
      assert.equal(result.skipped[0].reason,'download_canceled_or_interrupted');
      assert.deepEqual(await readdir(grant),[]);
      return;
    }
    assert.deepEqual(await readFile(original),bytes);
    assert.equal(result.count,1);
    const [step]=result.steps;
    assert.deepEqual(step.action,{type:'save_as',trigger:'click',source:'download'});
    assert.deepEqual(step.locator,{role:'link',name:'Get report'});
    for (const path of [join(out,'steps.jsonl'), join(out,'evidence/actions.jsonl'), join(out,'walkthrough.md'),
      result.saved.recording_path, result.saved.workflow_path]) {
      const artifact=await readFile(path,'utf8');
      assert.ok(!artifact.includes(grant) && !artifact.includes('private-original.bin'),path);
    }
    const destination=join(grant,'new destination.bin');
    const replay=(...flags)=>run('run','download-report','--permission','desktop.input','--permission','desktop.observe',
      '--inputs',JSON.stringify({[step.input.name]:destination}),...flags);
    await assert.rejects(replay(),/POLICY.DENIED/);
    assert.equal(requests,1);
    payload=Buffer.from(bytes).reverse();
    assert.equal((await replay('--allow-save-dir',grant)).status,'succeeded');
    assert.equal(requests,2);
    assert.deepEqual(await readFile(destination),payload);
    assert.deepEqual(await readFile(original),bytes);
  });
}
