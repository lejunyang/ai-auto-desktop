import assert from 'node:assert/strict';
import {execFile, spawn} from 'node:child_process';
import {writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {createInterface} from 'node:readline';
import {test} from 'node:test';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

const script = `
const submit = {name: "Save", role: "button"};
const field = {name: "Draft"};
const receipts = [];
for (const id of inputs.ids) {
    page.fill(field, id);
    if (page.read(field) !== id) throw new Error("wrong draft");
    page.click(submit);
    receipts.push(page.read({name: "Receipt"}));
}
return {receipts, count: receipts.length};
`;

test('browser JavaScript uses one page for branching, input, readback and cancellation',
  {skip: !process.env.AAD_TEST_CHROME, timeout: 60000}, async t => {
    const page = await fixture((_req, res) => {
      res.setHeader('Content-Type', 'text/html');
      res.end('<input aria-label="Draft"><input aria-label="Receipt" readonly><button>Save</button><script>window.orders=[];const [draft,receipt]=document.querySelectorAll("input"); document.querySelector("button").onclick=()=>{orders.push(draft.value);receipt.value=draft.value}</script>');
    });
    t.after(() => page.close());
    const cli = process.env.AAD_TEST_CLI || resolve('target/debug/aad');
    const env = {...process.env, AAD_CONTROL_LEASE_DIR: join(page.root,'leases')};
    const file = join(page.root, 'orders.js');
    await writeFile(file, script);
    const args = ['browser-script', file, '--url', page.url, '--browser-port', String(page.port),
      '--inputs', JSON.stringify({ids: ['bill-1','bill-2']})];
    await assert.rejects(promisify(execFile)(cli,args,{env}),
      err => JSON.parse(err.stdout).error.code === 'POLICY.DENIED');
    assert.deepEqual(await page.cdp.eval('window.orders'), []);
    const {stdout} = await promisify(execFile)(cli,[...args,'--allow-input'],{env});
    assert.deepEqual(JSON.parse(stdout).result, {receipts: ['bill-1','bill-2'], count: 2});
    assert.deepEqual(await page.cdp.eval('window.orders'), ['bill-1','bill-2']);

    const child = spawn(cli,['mcp','--browser-port',String(page.port),'--provider','browser.cdp','--allow-input'],
      {env,stdio:['pipe','pipe','inherit']});
    t.after(() => { child.stdin.end(); child.kill(); });
    const lines = createInterface({input:child.stdout})[Symbol.asyncIterator]();
    let id = 0;
    const send = msg => child.stdin.write(JSON.stringify({jsonrpc:'2.0',...msg})+'\n');
    const start = (source, options={}) => send({id:++id,method:'tools/call',
      params:{name:'browser_script',arguments:{source,url:page.url,...options}}});
    async function reply(expectError) {
      const message = JSON.parse((await lines.next()).value).result;
      assert.equal(message.isError,expectError,JSON.stringify(message));
      return JSON.parse(message.content[0].text);
    }
    start('return page.read({name:"Receipt"})');
    assert.equal((await reply(false)).result,'bill-2');
    start('page.click({name:"Save"}); const invalid = ;');
    assert.equal((await reply(true)).completed_actions,0);
    start('page.click({name:"Save"}); page.click({name:"Missing"}); page.click({name:"Save"});');
    assert.equal((await reply(true)).completed_actions,1);
    assert.deepEqual(await page.cdp.eval('window.orders'), ['bill-1','bill-2','bill-2']);
    start('while (true) {}',{timeout_ms:200});
    assert.equal((await reply(true)).error.code,'CONTROL.TIMEOUT');
    start('page.fill({name:"Draft"},"waiting");page.wait({name:"Not present"}); page.click({name:"Save"});');
    for (let i=0; i<100 && await page.cdp.eval('draft.value') !== 'waiting'; i++)
      await new Promise(resolve => setTimeout(resolve,20));
    assert.equal(await page.cdp.eval('draft.value'),'waiting');
    send({method:'notifications/cancelled',params:{requestId:id}});
    assert.equal((await reply(true)).error.code,'CONTROL.CANCELLED');
    assert.equal(await page.cdp.eval('window.orders.length'),3);
});
