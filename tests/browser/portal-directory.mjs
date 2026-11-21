import assert from 'node:assert/strict';
import {execFile, spawn} from 'node:child_process';
import {once} from 'node:events';
import {mkdir} from 'node:fs/promises';
import {join} from 'node:path';
import {setTimeout as delay} from 'node:timers/promises';
import {fileURLToPath} from 'node:url';
import {promisify} from 'node:util';

const exec = promisify(execFile);

// Observe the selected URI on this test's private bus. Chromium can present
// its own site permission prompt after the system file window has closed.
export async function verifyDirectory(page, root, handle, t) {
  const directory = join(root, 'reports 账单');
  await mkdir(directory);
  const args = ['--compact', 'browser', 'choose-directory', '--page', page.target.id,
    '--cdp-port', String(page.port), '--allow-input', '--directory', directory, '--allow-directory'];
  const run = grant => exec(process.env.AAD_TEST_CLI, [...args, grant],
    {env: {...process.env, AAD_CONTROL_LEASE_DIR: join(root, 'leases')}});
  const other = join(root, 'other');
  await mkdir(other);
  await assert.rejects(run(other));
  const monitor = spawn('dbus-monitor', ['--session',
    `type='signal',interface='org.freedesktop.portal.Request',member='Response',path='${handle}'`]);
  t.after(() => monitor.kill());
  let output = '';
  monitor.stdout.on('data', chunk => { output += chunk; });
  await once(monitor.stdout, 'data', {signal: AbortSignal.timeout(5000)});
  const result = JSON.parse((await run(root)).stdout);
  assert.equal(result.directory_selected, true);
  assert.equal(result.picker_closed, true);
  assert.equal(result.path, directory);
  for (let i = 0; i < 100 && !output.includes('file://'); i++) await delay(20);
  const uris = [...output.matchAll(/string "(file:\/\/[^"]+)"/g)].map(m => fileURLToPath(m[1]));
  assert.deepEqual(uris, [directory], output);
  assert.match(output, /member=Response\s+uint32 0/);
}
