import assert from 'node:assert/strict';
import {execFile, spawn} from 'node:child_process';
import {once} from 'node:events';
import {mkdtemp, rm, writeFile} from 'node:fs/promises';
import {createServer} from 'node:http';
import {tmpdir} from 'node:os';
import {join, resolve} from 'node:path';
import {test} from 'node:test';
import {setTimeout as delay} from 'node:timers/promises';
import {promisify} from 'node:util';

const run = promisify(execFile);
const options = {skip: process.platform !== 'linux' || !process.env.AAD_TEST_FIREFOX, timeout: 45000};

async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), 'aad-firefox-popup-'));
  const events = [];
  const server = createServer((req, res) => {
    res.setHeader('Content-Type', 'text/html');
    if (req.url.startsWith('/event/')) {
      events.push(req.url.slice(7));
      return res.end();
    }
    const page = {
      '/': `<input aria-label="Receipt" value="pending">
        <button onclick="log('choose');window.open('/provider')">Choose provider</button>
        <button onclick="log('unbound');window.open('/verify','_blank','noopener')">No opener</button>
        <button onclick="log('other');frames[0].launch()">Other opener</button>
        <iframe src="/frame" style="position:absolute;left:16px;top:100px;width:500px;height:120px;border:0"></iframe>`,
      '/frame': `<input aria-label="Receipt" value="frame pending">
        <button onclick="log('frame-open');window.open('/verify')">Frame authorize</button>
        <script>window.launch=()=>open('/verify')</script>`,
      '/provider': `<input aria-label="Provider receipt" value="pending">
        <button onclick="log('verify');window.open('/verify')">Verify identity</button>
        <button onclick="opener.document.querySelector('input').value=document.querySelector('input').value;log('returned')">Return receipt</button>`,
      '/verify': `<button onclick="opener.document.querySelector('input').value='receipt-735';log('confirmed');window.close()">Confirm</button>
        <button onclick="log('wrong-action')">Forbidden</button>`
    }[req.url];
    res.end('<!doctype html><script>function log(s){navigator.sendBeacon("/event/"+s)}</script>'
      + (page || '') + '<script>log("ready"+location.pathname)</script>');
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const url = 'http://127.0.0.1:' + server.address().port + '/';
  const env = {...process.env, AAD_CONTROL_LEASE_DIR:join(root, 'leases')};
  delete env.DISPLAY;
  delete env.WAYLAND_DISPLAY;
  const firefox = spawn(process.env.AAD_TEST_FIREFOX,
    ['--headless','--no-remote','--profile',root,'--remote-debugging-port','0',url],
    {env, detached:true, stdio:['ignore','ignore','pipe']});
  let stderr = '';
  firefox.stderr.on('data', b => { stderr += b; });
  t.after(async () => {
    try { process.kill(-firefox.pid, 'SIGTERM'); } catch {}
    await Promise.race([once(firefox, 'exit'), delay(2000)]);
    try { process.kill(-firefox.pid, 'SIGKILL'); } catch {}
    server.closeAllConnections();
    server.close();
    await rm(root, {recursive:true, force:true});
  });
  for (let i = 0; i < 200 && !events.includes('ready/frame'); i++) await delay(50);
  assert.ok(events.includes('ready/frame'), stderr);
  const port = stderr.match(/WebDriver BiDi listening on ws:\/\/127\.0\.0\.1:(\d+)/)?.[1];
  assert.ok(port, stderr);
  return {events, async script(source, {allow = true, timeout = 12000, path = ''} = {}) {
    const file = join(root, 'popup.js');
    await writeFile(file, source);
    const args = ['browser','script',file,'--url',url+path,'--browser-bidi-port',port,
      '--inputs',JSON.stringify({provider:url+'provider', verify:url+'verify'}),'--timeout-ms',String(timeout)];
    if (allow) args.push('--allow-input');
    try {
      return JSON.parse((await run(process.env.AAD_TEST_CLI || resolve('target/debug/aad'), args, {env})).stdout);
    } catch (err) {
      if (!err.stdout) throw err;
      return JSON.parse(err.stdout);
    }
  }};
}

test('Firefox binds nested popups and reads the original receipt', options, async t => {
  const app = await fixture(t);
  const source = `page.openPopup({name:'Choose provider'},inputs.provider);
    page.openPopup({name:'Verify identity'},inputs.verify);
    page.click({name:'Confirm'});
    page.returnToOpener();
    const provider=page.read({name:'Provider receipt'});
    page.click({name:'Return receipt'});
    page.returnToOpener();
    return [provider,page.read({name:'Receipt'})];`;
  const denied = await app.script(source, {allow:false});
  assert.equal(denied.error?.code, 'POLICY.DENIED', JSON.stringify(denied));
  assert.ok(!app.events.includes('choose'));
  const reply = await app.script(source);
  assert.deepEqual(reply.result, ['receipt-735','receipt-735'], JSON.stringify({reply, events:app.events}));
  assert.equal(reply.completed_actions, 4);
  assert.deepEqual(app.events.filter(e => !e.startsWith('ready')), ['choose','verify','confirmed','returned']);
});

test('Firefox returns to the initiating iframe', options, async t => {
  const app = await fixture(t);
  const reply = await app.script(`page.openPopup({name:'Frame authorize'},inputs.verify);
    page.click({name:'Confirm'}); page.returnToOpener(); return page.read({name:'Receipt'});`, {path:'frame'});
  assert.equal(reply.result, 'receipt-735', JSON.stringify({reply,events:app.events}));
  assert.equal((await app.script("return page.read({name:'Receipt'})")).result, 'pending');
  assert.deepEqual(app.events.filter(e => !e.startsWith('ready')), ['frame-open','confirmed']);
});

for (const [name, event] of [['No opener','unbound'],['Other opener','other']]) {
  test('Firefox ignores a new matching URL from ' + name, options, async t => {
    const app = await fixture(t);
    const reply = await app.script(`page.openPopup({name:${JSON.stringify(name)}}, inputs.verify);
      page.click({name:'Forbidden'});`, {timeout:3500});
    assert.equal(reply.error?.code, 'CONTROL.TIMEOUT', JSON.stringify(reply));
    assert.equal(reply.completed_actions, 1);
    assert.ok(app.events.includes('ready/verify'));
    assert.deepEqual(app.events.filter(e => !e.startsWith('ready')), [event]);
  });
}
