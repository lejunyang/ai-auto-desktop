import assert from 'node:assert/strict';
import {execFile, spawn} from 'node:child_process';
import {once} from 'node:events';
import {mkdir, mkdtemp, readFile, rm, symlink, unlink, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {setTimeout as delay} from 'node:timers/promises';
import {fileURLToPath} from 'node:url';
import {promisify} from 'node:util';
import {clickPickerButton, fixture} from './fixture.mjs';

const exec = promisify(execFile);

// Observe the selected URI on this test's private bus. Chromium can present
// its own site permission prompt after the system file window has closed.
export async function verifyDirectory(page, root, handle, t) {
  const directory = join(root, 'reports 账单');
  await mkdir(directory);
  await writeFile(join(directory, 'marker.txt'), 'directory upload fixture');
  const args = ['--compact', 'browser', 'choose-directory', '--page', page.target.id,
    '--cdp-port', String(page.port), '--allow-input', '--directory', directory, '--allow-directory'];
  const run = grant => exec(process.env.AAD_TEST_CLI, [...args, grant],
    {env: {...process.env, AAD_CONTROL_LEASE_DIR: join(root, 'leases')}});
  const other = join(root, 'other');
  await mkdir(other);
  await assert.rejects(run(other));
  const result = await selectedPath(handle, directory, t, () => run(root));
  assert.equal(result.path, directory);
}

async function selectedPath(handle, expected, t, run) {
  const monitor = spawn('dbus-monitor', ['--session',
    `type='signal',interface='org.freedesktop.portal.Request',member='Response',path='${handle}'`]);
  t.after(() => monitor.kill());
  let output = '';
  monitor.stdout.on('data', chunk => { output += chunk; });
  await once(monitor.stdout, 'data', {signal: AbortSignal.timeout(5000)});
  const result = JSON.parse((await run()).stdout);
  assert.equal(result.directory_selected, true);
  assert.equal(result.picker_closed, true);
  for (let i = 0; i < 100 && !/string "file:\/\/[^"]+"/.test(output); i++) await delay(20);
  const uris = [...output.matchAll(/string "(file:\/\/[^"]+)"/g)].map(m => fileURLToPath(m[1]));
  assert.deepEqual(uris, [expected], output);
  assert.match(output, /member=Response\s+uint32 0/);
  return result;
}

export async function verifyFolderUpload(t, getHandle) {
  const root = await mkdtemp(join(tmpdir(), 'folder-upload-'));
  t.after(() => rm(root, {recursive:true, force:true}));
  const page = await fixture((_request, response) => {
    response.setHeader('Content-Type', 'text/html');
    response.end('<input type=file webkitdirectory id=folder>');
  }, ['--ozone-platform=' + process.env.XDG_SESSION_TYPE], {headed:true});
  t.after(() => page.close());
  await clickPickerButton(page, 'select-upload', 'folder.click()');
  const handle = await getHandle();
  assert.equal(await page.cdp.eval('folder.matches(":open")'), true);
  const source = join(root, 'reports'), stage = join(root, 'snapshot');
  await mkdir(source);
  const marker = join(source, 'marker.txt');
  await writeFile(marker, 'original bytes');
  const run = (...args) => exec(process.env.AAD_TEST_CLI,
    ['--compact','browser','choose-directory','--page',page.target.id,'--cdp-port',String(page.port),
      '--allow-input', ...args], {env:{...process.env, AAD_CONTROL_LEASE_DIR:join(root,'leases')}});
  const upload = () => run('--allow-upload-tree',source,'--upload-tree-stage',stage);
  await assert.rejects(run('--allow-directory',root,'--directory',source));
  await symlink(root, join(source,'link'));
  await assert.rejects(upload());
  assert.equal(await page.cdp.eval('folder.files.length'),0);
  await unlink(join(source,'link'));
  const snapshot = join(stage,'reports');
  const result = await selectedPath(handle, snapshot, t, upload);
  assert.equal(result.snapshot, snapshot);
  await writeFile(marker, 'changed after selection');
  assert.equal(await readFile(join(snapshot,'marker.txt'),'utf8'),'original bytes');
  assert.ok(['pending','verified'].includes(result.file_list), result.file_list);
  if (result.file_list === 'verified') {
    assert.deepEqual(await page.cdp.eval('Promise.all(Array.from(folder.files, async f => [f.webkitRelativePath, await f.text()]))'),
      [['reports/marker.txt','original bytes']]);
  }
  t.diagnostic('Chromium FileList at system chooser closure: '+result.file_list);
}
