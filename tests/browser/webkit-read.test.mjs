import assert from 'node:assert/strict';
import {execFile, spawn} from 'node:child_process';
import {once} from 'node:events';
import {createServer} from 'node:http';
import {resolve} from 'node:path';
import {test} from 'node:test';
import {setTimeout as delay} from 'node:timers/promises';
import {promisify} from 'node:util';

test('WebKit reads rendered text and named controls, and closes the session after a redirect', {
  skip: !process.env.AAD_TEST_WEBKIT_DRIVER, timeout: 120000,
}, async t => {
  const fixture = createServer((req, res) => {
    if (req.url === '/redirect') {
      res.writeHead(302, {Location:'/unexpected'}).end();
      return;
    }
    res.setHeader('Content-Type', 'text/html; charset=utf-8');
    if (req.url === '/long') {
      res.end('<button aria-label="'+'A'.repeat(300)+'"></button>');
      return;
    }
    res.end(`<h1>账单</h1><span hidden>hidden-marker</span><p>账单号 12345</p>
      <textarea>private-draft</textarea><input type=password value="private-password">
      <a href="/bill">详情</a><button aria-label="打印账单">打印</button>`);
  });
  fixture.listen(0, '127.0.0.1');
  await once(fixture, 'listening');
  t.after(() => new Promise(done => fixture.close(done)));
  const url = 'http://127.0.0.1:' + fixture.address().port + '/';
  const socket = createServer().listen(0, '127.0.0.1');
  await once(socket, 'listening');
  const port = socket.address().port;
  await new Promise(done => socket.close(done));
  const driver = spawn(process.env.AAD_TEST_WEBKIT_DRIVER, ['--port='+port], {stdio:'ignore'});
  t.after(() => driver.kill());
  let ready = false;
  for (let i = 0; i < 100 && !ready; i++) {
    ready = await fetch('http://127.0.0.1:'+port+'/status').then(r => r.ok, () => false);
    if (!ready) await delay(50);
  }
  assert.ok(ready, 'WebKit driver did not start');
  const read = async (target, ...options) => {
    const {stdout} = await promisify(execFile)(process.env.AAD_TEST_CLI || resolve('target/debug/aad'),
      ['browser','read','--url',target,'--webkit-port',String(port),...options]);
    return JSON.parse(stdout);
  };
  const page = await read(url);
  assert.equal(page.url, url);
  assert.equal(page.truncated, false);
  assert.match(page.text, /账单号 12345/);
  assert.doesNotMatch(page.text, /hidden-marker|private-/);
  await assert.rejects(read(url+'redirect'), e => {
    const result = JSON.parse(e.stdout);
    return result.code === 'BROWSER.WEBKIT_READ_FAILED' && result.message.includes('requested URL');
  });
  const outline = await read(url, '--view','outline');
  assert.deepEqual(outline.nodes, [
    {role:'heading',name:'账单'}, {role:'link',name:'详情'}, {role:'button',name:'打印账单'},
  ]);
  const long = await read(url+'long', '--view','outline');
  assert.equal(long.truncated, true);
  assert.equal(long.nodes[0].name, 'A'.repeat(256));
});
