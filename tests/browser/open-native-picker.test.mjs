import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {mkdir, readFile, symlink, writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {test} from 'node:test';
import {setTimeout as delay} from 'node:timers/promises';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

const exec = promisify(execFile);
const active = async () => (await exec('xdotool', ['getactivewindow'])).stdout.trim();
async function waitDialog(owner) {
  // The first chooser can spend 25 seconds waiting for the desktop portal.
  for (let i = 0; i < 1200; i++) {
    const xid = await active();
    if (xid !== owner) {
      const {stdout} = await exec('xprop', ['-id', xid, '_NET_WM_WINDOW_TYPE']);
      if (stdout.includes('_NET_WM_WINDOW_TYPE_DIALOG')) return xid;
    }
    await delay(50);
  }
  assert.fail('the headed browser did not map a native dialog');
}

const gtk = process.env.AAD_TEST_PICKER_GTK || '3';
const portal = process.env.AAD_TEST_PICKER_PORTAL === '1';
test(`complete an already-open ${portal ? 'GTK portal' : 'GTK '+gtk} X11 file picker on the observed page`, {
  skip: !process.env.AAD_TEST_PICKER_X11 || !process.env.AAD_TEST_CHROME, timeout: 150_000
}, async t => {
  const page = await fixture((_req, res) => {
    res.setHeader('Content-Type', 'text/html');
    res.end('<input type=file id=files hidden multiple><input type=file id=sibling><input id=text value=original>');
  }, ['--ozone-platform=x11', '--gtk-version='+gtk], {headed:true});
  t.after(() => page.close());
  const allowed = join(page.root, 'grant');
  await mkdir(allowed);
  const first = join(allowed, 'one file.txt'), second = join(allowed, 'two.txt');
  const outside = join(page.root, 'outside.txt'), escape = join(allowed, 'link');
  await Promise.all([writeFile(first, 'payload one'), writeFile(second, 'payload two'), writeFile(outside, 'original outside')]);
  await symlink(outside, escape);
  const cli = process.env.AAD_TEST_CLI || resolve('target/debug/aad');
  const run = (pageId, ...args) => exec(cli, ['--compact', 'do', 'choose-files',
    '--cdp-port', String(page.port), '--page', pageId, '--allow-input', '--allow-upload-dir', allowed, ...args],
    {env:{...process.env, AAD_CONTROL_LEASE_DIR:join(page.root, 'leases')}});
  const reject = async (promise, code) => assert.rejects(promise, e => {
    const reply = JSON.parse(e.stdout);
    return reply.error.code === code;
  });
  const {targetId: other} = await page.cdp.call('Target.createTarget', {url:page.url + '?other', background:true});
  const owner = await active();
  await page.cdp.eval('window.events=[]; for(const type of ["cancel","change"]) files.addEventListener(type,e=>events.push([type,e.isTrusted]))');
  const gesture = expression => page.cdp.call('Runtime.evaluate', {expression, userGesture:true});
  await gesture('files.click()');
  const picker = await waitDialog(owner);
  const {stdout:properties} = await exec('xprop', ['-id', picker, 'WM_WINDOW_ROLE', '_NET_WM_PID']);
  const opened = () => page.cdp.eval('[files.matches(":open"), sibling.matches(":open"), files.files.length, sibling.files.length]');
  assert.deepEqual(await opened(), [true, false, 0, 0]);
  await reject(run(page.target.id, '--window', owner, '--file', first), 'CLI.OPEN_PICKER');
  await reject(run(other, '--file', first), 'CONTROL.NOT_FOUND');
  await reject(run(page.target.id, '--file', first, '--file', escape), 'POLICY.DENIED');
  assert.equal(await active(), picker);
  assert.deepEqual(await opened(), [true, false, 0, 0]);
  if (portal) {
    const secondBrowser = await fixture((_req, res) => {
      res.setHeader('Content-Type','text/html');
      res.end('<input id=f type=file>');
    }, ['--ozone-platform=x11'], {headed:true});
    try {
      const secondOwner = await active();
      await secondBrowser.cdp.call('Runtime.evaluate', {expression:'f.click()', userGesture:true});
      const otherDialog = await waitDialog(secondOwner);
      assert.notEqual(otherDialog, picker);
      await reject(run(page.target.id, '--file', first), 'CLI.OPEN_PICKER');
      await reject(exec(cli, ['--compact','do','choose-files','--cdp-port',String(secondBrowser.port),
        '--page',secondBrowser.target.id,'--allow-input','--allow-upload-dir',allowed,'--file',first]), 'CLI.OPEN_PICKER');
      assert.equal(await active(), otherDialog);
      assert.deepEqual(await secondBrowser.cdp.eval('[f.matches(":open"), f.files.length]'), [true,0]);
      assert.deepEqual(await opened(), [true,false,0,0]);
    } finally { await secondBrowser.close(); }
    await exec('xdotool', ['windowactivate','--sync',picker]);
  }
  const {stdout} = await run(page.target.id, '--file', second, '--file', first);
  const result = JSON.parse(stdout);
  assert.equal(String(result.window), picker);
  const dialogPid = Number(properties.match(/_NET_WM_PID\(CARDINAL\) = (\d+)/)[1]);
  assert.equal(dialogPid === result.browser_pid, !portal);
  assert.equal(properties.includes('"GtkFileChooserDialog"'), portal || gtk === '3');
  assert.equal(result.picker_closed, true);
  assert.equal(result.file_count, 2);
  assert.equal(await active(), owner);
  const actual = await page.cdp.eval('Promise.all(Array.from(files.files, async f=>[f.name, await f.text()]))');
  assert.deepEqual(actual, [['two.txt', 'payload two'], ['one file.txt', 'payload one']]);
  assert.deepEqual(await page.cdp.eval('[events,sibling.files.length, text.value]'), [[['change', true]], 0, 'original']);

  await gesture('void showSaveFilePicker({suggestedName:"outside.txt"}).catch(()=>{})');
  const save = await waitDialog(owner);
  await reject(run(page.target.id, '--window', save, '--file', first), 'CLI.OPEN_PICKER');
  if (gtk === '4' && !portal) {
    await exec('xprop', ['-id', save, '-f', '_NET_WM_NAME', '8u', '-set', '_NET_WM_NAME', 'Open File']);
    await reject(run(page.target.id, '--window', save, '--file', first), 'CLI.OPEN_PICKER');
  }
  assert.equal(await active(), save);
  assert.equal(await readFile(outside, 'utf8'), 'original outside');
});
