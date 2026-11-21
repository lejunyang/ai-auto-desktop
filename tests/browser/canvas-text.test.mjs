import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {test} from 'node:test';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

test('read and use painted labels on a free-layout canvas', {
  skip: process.platform !== 'linux' || !process.env.AAD_TEST_CHROME, timeout: 90000
}, async t => {
  const page = await fixture((_req, res) => {
    res.setHeader('Content-Type', 'text/html');
    res.end(`<!doctype html><canvas aria-label="Diagram" width=650 height=240></canvas>
      <script>
      const el=document.querySelector('canvas'), ctx=el.getContext('2d');
      ctx.fillStyle='white'; ctx.fillRect(0,0,el.width,el.height);
      ctx.fillStyle='#151515'; ctx.font='bold 32px sans-serif';
      ctx.fillText('START',30,48); ctx.fillText('REVIEW',393,169);
      ctx.fillText('END',132,219);
      el.onclick=e=>window.hit=e.isTrusted && e.offsetX>=393 && e.offsetX<535 &&
        e.offsetY>141 && e.offsetY<169;
      </script>`);
  });
  t.after(() => page.close());
  const cli = process.env.AAD_TEST_CLI || resolve('target/debug/aad');
  const args = ['--browser-port',String(page.port),'--url',page.url];
  const env = {...process.env, AAD_CONTROL_LEASE_DIR:join(page.root, 'leases')};
  const run = async (...flags) => JSON.parse((await promisify(execFile)(cli, flags, {env})).stdout);
  const read = () => run('browser','read',...args,'--canvas-text','{"name":"Diagram"}','--language','en-US');
  const result = await read();
  const words = result.lines.flatMap(line => line.words);
  assert.deepEqual(words.map(w => w.text).sort(), ['END','REVIEW','START']);
  const review = words.find(w => w.text === 'REVIEW');
  assert.ok(review.confidence >= 0.80);
  assert.match(result.digest, /^[0-9a-f]{64}$/);
  const script = join(page.root, 'review.js');
  await writeFile(script, `const locator={name:"Diagram"};
    const words=page.readText(locator, "en-US").lines.flatMap(line=>line.words);
    const matches=words.filter(w=>w.text==="REVIEW" && w.confidence>=0.80);
    if(matches.length!==1) throw Error("expected one REVIEW label");
    const {fx,fy,fw,fh}=matches[0].box;
    return page.act(locator,{type:"click",fraction:{fx:fx+fw/2,fy:fy+fh/2}});`);
  await run('browser','script',script,...args,'--allow-input');
  assert.equal(await page.cdp.eval('window.hit'), true);
  await page.cdp.eval("el.insertAdjacentHTML('afterend','<input type=password style=\"position:fixed;left:25px;top:25px\">');true");
  await assert.rejects(read(), err => JSON.parse(err.stdout).code === 'CONTROL.ACTION_UNSUPPORTED');
  await page.cdp.eval("document.querySelector('input').remove();ctx.fillStyle='white';ctx.fillRect(0,0,el.width,el.height);true");
  const blank = await read();
  assert.deepEqual(blank.lines, []);
  assert.notEqual(blank.digest, result.digest);
});
