import assert from 'node:assert/strict';
import {execFile, spawn} from 'node:child_process';
import {mkdir, writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {test} from 'node:test';
import {createInterface} from 'node:readline';
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

test('closing the native picker invalidates a previously observed open file input', {
  skip: !process.env.AAD_TEST_CHROME || !process.env.AAD_TEST_PICKER_X11, timeout: 90000
}, async t => {
  const page = await fixture((_req, res) => {
    res.setHeader('Content-Type', 'text/html');
    res.end('<title>Open picker fixture</title><input type="file" id="invoice"><input type="file" id="other">');
  }, ['--ozone-platform=x11'], {headed:true});
  t.after(() => page.close());
  const file = join(page.root, 'invoice.txt');
  await writeFile(file, 'invoice payload');
  const cli = process.env.AAD_TEST_CLI || resolve('target/debug/aad');
  const child = spawn(cli, ['mcp', '--browser-port', String(page.port), '--provider', 'browser.cdp',
    '--allow-input', '--allow-upload-dir', page.root], {
    stdio: ['pipe','pipe','inherit'], env:{...process.env,AAD_CONTROL_LEASE_DIR:join(page.root,'leases')}
  });
  t.after(() => { child.stdin.end(); child.kill(); });
  const lines = createInterface({input:child.stdout})[Symbol.asyncIterator]();
  let id=0;
  const tool = async (name, args, expectError=false) => {
    child.stdin.write(JSON.stringify({jsonrpc:'2.0',id:++id,method:'tools/call',params:{name,arguments:args}})+'\n');
    const reply=JSON.parse((await lines.next()).value).result;
    assert.equal(reply.isError, expectError, JSON.stringify(reply));
    return JSON.parse(reply.content[0].text);
  };
  const resources = (await tool('control_targets', {})).data.resources;
  const resource_id = resources.find(r=>r.metadata.url === page.url).resource_id;
  await page.cdp.call('Runtime.evaluate', {expression:'invoice.click()',userGesture:true});
  for (let i=0; i<600; i++) {
    const xid=(await exec('xdotool',['getactivewindow'])).stdout.trim();
    if ((await exec('xprop',['-id',xid,'_NET_WM_WINDOW_TYPE'])).stdout.includes('_NET_WM_WINDOW_TYPE_DIALOG')) break;
    if (i===599) assert.fail('native picker did not appear');
    await delay(50);
  }
  const {observation_id}=(await tool('control_observe',{resource_id})).data;
  const {target}=(await tool('control_find',{observation_id,locator:{states:{picker_open:true}}})).data;
  await exec('xdotool',['key','Escape']);
  for (let i=0; i<100 && await page.cdp.eval('invoice.matches(":open")'); i++) await delay(30);
  assert.equal(await page.cdp.eval('invoice.matches(":open")'), false);
  const refused=await tool('control_act',{target,action:{type:'upload',files:[file]}},true);
  assert.equal(refused.error.code,'CONTROL.STALE_TARGET');
  assert.deepEqual(await page.cdp.eval('[invoice.files.length,other.files.length]'),[0,0]);
});
