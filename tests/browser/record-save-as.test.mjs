import assert from 'node:assert/strict';
import {execFile, spawn} from 'node:child_process';
import {once} from 'node:events';
import {mkdtemp, readFile, rm, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {basename, join, resolve} from 'node:path';
import {test} from 'node:test';
import {setTimeout as delay} from 'node:timers/promises';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

const cli = process.env.AAD_TEST_CLI || resolve('target/debug/aad');
const exec = promisify(execFile);

async function until(probe, description) {
  for (let i = 0; i < 200; i++) {
    if (await probe()) return;
    await delay(50);
  }
  assert.fail('Timed out waiting for ' + description);
}

async function recording(t, headed) {
  const page = await fixture((_, res) => {
    res.setHeader('Content-Type', 'text/html');
    res.end(`<button onclick="save()">Export ledger</button><script>
      window.attempts = 0; window.contents = 'recorded ledger bytes';
      async function save() {
        window.attempts++;
        try {
          const handle = await showSaveFilePicker({suggestedName: 'ledger.txt'});
          const writer = await handle.createWritable();
          await writer.write(window.contents); await writer.close();
          window.receipt = [handle.name, await (await handle.getFile()).text()];
        } catch (err) { window.failure = err.name; }
      }
    </script>`);
  }, headed ? ['--ozone-platform=' + process.env.XDG_SESSION_TYPE,
    ...(process.env.AAD_TEST_DIRECT_GTK4_SAVE === '1' ? ['--gtk-version=4'] : [])] : [], {headed});
  t.after(() => page.close());
  const out = join(page.root, 'capture');
  const env = {...process.env, AAD_CDP_PORT: String(page.port),
    AAD_RECORDINGS_DIR: join(page.root, 'store'), AAD_CONTROL_LEASE_DIR: join(page.root, 'leases')};
  const run = async (...args) => JSON.parse((await exec(cli, args, {env})).stdout);
  const proc = spawn(cli, ['browser-record', 'start', '--page', page.target.id,
    '--cdp-port', String(page.port), '--out', out, '--no-screenshots'], {env, stdio:'ignore'});
  const exited = once(proc, 'exit');
  t.after(() => proc.kill());
  await until(async () => {
    if (proc.exitCode !== null) assert.fail('recorder exited ' + proc.exitCode);
    try { return JSON.parse(await readFile(join(out, 'manifest.json'))).status === 'recording'; }
    catch { return false; }
  }, 'recorder startup');
  const click = async () => {
    const point = await page.cdp.eval(`(() => {
      const b = document.querySelector('button'), r = b.getBoundingClientRect();
      const x = r.x + r.width/2, y = r.y + r.height/2;
      if (b.textContent !== 'Export ledger' || document.elementFromPoint(x,y) !== b) throw Error('trigger changed');
      return {x,y};
    })()`);
    for (const type of ['mousePressed','mouseReleased'])
      await page.cdp.call('Input.dispatchMouseEvent', {type,button:'left',clickCount:1,...point});
  };
  const stop = async () => {
    await run('browser-record','stop','--out',out);
    assert.equal((await exited)[0], 0);
  };
  return {page, out, run, click, stop};
}

test('headless Chromium cancellation consumes the initiating action and emits no save', {
  skip: !process.env.AAD_TEST_CHROME, timeout: 30000
}, async t => {
  const {page, out, run, click, stop} = await recording(t, false);
  await click();
  await until(() => page.cdp.eval("window.failure === 'AbortError'"), 'picker rejection');
  // Future documents get their own observer and call IDs. Enter on a button
  // causes both a keydown and a click, but only one native picker.
  await page.cdp.call('Page.navigate', {url: page.url + 'next'});
  await until(() => page.cdp.eval("location.pathname === '/next' && !!document.querySelector('button')").catch(() => false), 'new document');
  assert.ok(await page.cdp.eval(`(() => {
    const button = document.querySelector('button'); button.focus();
    return button.textContent === 'Export ledger' && document.activeElement === button;
  })()`));
  for (const type of ['keyDown','keyUp']) {
    await page.cdp.call('Input.dispatchKeyEvent', {type,key:'Enter',code:'Enter',windowsVirtualKeyCode:13,
      ...(type === 'keyDown' ? {text:'\r'} : {})});
  }
  await until(() => page.cdp.eval("window.failure === 'AbortError'"), 'keyboard picker rejection');
  await stop();
  const result = await run('browser-record','steps','--out',out);
  assert.deepEqual(result.steps, []);
  assert.deepEqual(result.skipped, Array(2).fill({reason:'save_picker_cancelled',noop:true}));
  assert.equal(await page.cdp.eval("Function.prototype.toString.call(showSaveFilePicker).includes('[native code]')"), true);
});

test('record and replay Save As with a fresh grant, real file handle and independent disk readback', {
  skip: !process.env.AAD_TEST_CHROME || (process.env.AAD_TEST_DIRECT_GTK4_SAVE !== '1' && process.env.AAD_TEST_FILE_PORTAL !== '1'), timeout: 120000
}, async t => {
  const {page, out, run, click, stop} = await recording(t, true);
  const active = async () => (await exec('xdotool',['getactivewindow'])).stdout.trim();
  const owner = process.env.XDG_SESSION_TYPE === 'x11' ? await active() : null;
  // Chromium protects its own profile directory from File System Access.
  const grant = await mkdtemp(join(tmpdir(),'aad-save-grant-'));
  t.after(() => rm(grant,{recursive:true,force:true}));
  const original = join(grant,'private-recorded-filename.txt');
  await click();
  if (owner) {
    await until(async () => (await active()) !== owner, 'native Save dialog');
  } else {
    await until(async () => {
      const backend = process.env.AAD_TEST_KDE_PICKER === '1' ? 'kde' : 'gtk';
      const {stdout} = await exec('busctl',['--user','--list','tree','org.freedesktop.impl.portal.desktop.' + backend]);
      return stdout.split('\n').some(s => /\/request\/[^/]+\/[^/]+$/.test(s));
    }, 'portal request');
  }
  await run('browser','save-as','--page',page.target.id,'--cdp-port',String(page.port),
    '--allow-input','--allow-save-dir',grant,'--file',original);
  await until(() => page.cdp.eval('!!(window.receipt || window.failure)'), 'page write and readback');
  assert.equal(await page.cdp.eval('window.failure ?? null'), null);
  assert.deepEqual(await page.cdp.eval('window.receipt'), [basename(original),'recorded ledger bytes']);
  assert.equal(await readFile(original,'utf8'), 'recorded ledger bytes');
  await stop();
  const result = await run('browser-record','steps','--out',out,'--save','export-ledger');
  assert.deepEqual(result.skipped, []);
  assert.equal(result.count, 1);
  const [step] = result.steps;
  assert.deepEqual(step.locator, {role:'button',name:'Export ledger'});
  assert.deepEqual(step.action, {type:'save_as',trigger:'click'});
  assert.deepEqual(step.input, {name:'rec1_destination',sensitive:false});
  for (const file of [join(out,'steps.jsonl'),join(out,'evidence','actions.jsonl'),join(out,'walkthrough.md'),
    result.saved.recording_path,result.saved.workflow_path]) {
    const text = await readFile(file, 'utf8');
    assert.ok(!text.includes(basename(original)) && !text.includes(grant), file);
  }
  const replay = (file, ...flags) => run('run','export-ledger','--permission','desktop.input',
    '--permission','desktop.observe','--inputs',JSON.stringify({[step.input.name]:file}),...flags);
  const dest = join(grant,'replayed ledger.txt');
  const denied = e => JSON.parse(e.stdout).error.code === 'POLICY.DENIED';
  await assert.rejects(replay(dest), denied);
  await assert.rejects(replay(join(page.root,'outside.txt'),'--allow-save-dir',grant), denied);
  await writeFile(original,'keep the original');
  await assert.rejects(replay(original,'--allow-save-dir',grant), denied);
  assert.equal(await page.cdp.eval('attempts'), 1);
  await page.cdp.eval("window.contents = 'fresh replay bytes'; window.receipt = null");
  const replayed = await replay(dest,'--allow-save-dir',grant);
  assert.equal(replayed.status,'succeeded');
  await until(() => page.cdp.eval('!!(window.receipt || window.failure)'), 'replayed page write and readback');
  assert.equal(await page.cdp.eval('window.failure ?? null'), null);
  assert.deepEqual(await page.cdp.eval('window.receipt'), [basename(dest),'fresh replay bytes']);
  assert.equal(await page.cdp.eval('attempts'), 2);
  assert.equal(await readFile(dest,'utf8'),'fresh replay bytes');
  assert.equal(await readFile(original,'utf8'),'keep the original');
});
