import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {mkdir, writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {test} from 'node:test';
import {setTimeout as delay} from 'node:timers/promises';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

const exec = promisify(execFile);

test('browser scripts select only granted files and the page receives their contents', {
  skip: !process.env.AAD_TEST_CHROME, timeout: 60000
}, async t => {
  const page = await fixture((_req, res) => {
    res.setHeader('Content-Type', 'text/html');
    res.end(`<input type="file" multiple aria-label="Attachments">
      <button onclick="openPicker()">Add invoice</button>
      <script>
        function openPicker() {
          window.opens = (window.opens || 0) + 1;
          const input = document.createElement("input");
          input.type = "file";
          input.hidden = true;
          input.multiple = true;
          input.onchange = async event => {
            if (!event.isTrusted) return;
            window.invoices = await Promise.all(Array.from(input.files, async f => [f.name, await f.text()]));
          };
          document.body.append(input);
          input.click();
        }
      </script>`);
  });
  t.after(() => page.close());
  const upload = join(page.root, 'allowed');
  await mkdir(upload);
  const first = join(upload, '1.txt');
  const second = join(upload, '2.txt');
  const outside = join(page.root, 'outside.txt');
  await Promise.all([writeFile(first, 'one'), writeFile(second, 'two'), writeFile(outside, 'other')]);
  const src = join(page.root, 'attach.js');
  await writeFile(src, 'return page.upload({name:"Attachments"}, inputs.files);');
  const cli = process.env.AAD_TEST_CLI || resolve('target/debug/aad');
  const call = (files, ...grants) => exec(cli, ['browser-script', src, '--url', page.url,
    '--browser-port', String(page.port), '--inputs', JSON.stringify({files}), ...grants], {
    env: {...process.env, AAD_CONTROL_LEASE_DIR: join(page.root, 'leases')}
  });
  const denied = err => JSON.parse(err.stdout).error.code === 'POLICY.DENIED';
  await assert.rejects(call([first], '--allow-input'), denied);
  await assert.rejects(call([first], '--allow-upload-dir', upload), denied);
  await assert.rejects(call([first, outside], '--allow-input', '--allow-upload-dir', upload), denied);
  assert.equal(await page.cdp.eval('document.querySelector("input").files.length'), 0);
  const result = await call([first, second], '--allow-input', '--allow-upload-dir', upload);
  assert.equal(JSON.parse(result.stdout).completed_actions, 1);
  const contents = await page.cdp.eval('Promise.all(Array.from(document.querySelector("input").files, async f => [f.name, await f.text()]))');
  assert.deepEqual(contents, [['1.txt', 'one'], ['2.txt', 'two']]);
  await writeFile(src, 'return page.chooseFiles({name:"Add invoice",role:"button"}, inputs.files);');
  await assert.rejects(call([outside], '--allow-input', '--allow-upload-dir', upload), denied);
  assert.equal(await page.cdp.eval('window.opens || 0'), 0);
  const chosen = await call([second, first], '--allow-input', '--allow-upload-dir', upload);
  assert.equal(JSON.parse(chosen.stdout).completed_actions, 1);
  assert.equal(await page.cdp.eval('window.opens'), 1);
  for (let i=0; i<100 && !await page.cdp.eval('Array.isArray(window.invoices)'); i++) await delay(20);
  assert.deepEqual(await page.cdp.eval('window.invoices'), [['2.txt', 'two'], ['1.txt', 'one']]);
});
