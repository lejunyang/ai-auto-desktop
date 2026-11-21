import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {test} from 'node:test';
import {setTimeout as delay} from 'node:timers/promises';
import {promisify} from 'node:util';
import {verifyFolderUpload} from './portal-directory.mjs';

const exec = promisify(execFile);

test('choose a private snapshot in an already-open HTML folder picker', {
  skip:process.env.AAD_TEST_FILE_PORTAL !== '1' || !process.env.AAD_TEST_CHROME,
  timeout:60000
}, async t => {
  await verifyFolderUpload(t, async () => {
    for (let i = 0; i < 150; i++) {
      const {stdout} = await exec('busctl', ['--address=' + process.env.DBUS_SESSION_BUS_ADDRESS,
        '--list', 'tree', 'org.freedesktop.portal.Desktop']);
      const requests = stdout.trim().split('\n').filter(row => /\/request\/[^/]+\/[^/]+$/.test(row));
      if (requests.length === 1) return requests[0];
      await delay(50);
    }
    assert.fail('private portal did not receive one folder chooser request');
  });
});
