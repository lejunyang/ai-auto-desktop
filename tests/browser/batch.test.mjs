import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {readFile, readdir} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {createInterface} from 'node:readline';
import {test} from 'node:test';
import {fixture} from './fixture.mjs';

test('typed batches re-observe after a write and stop on a missing element',
  {skip: !process.env.AAD_TEST_CHROME, timeout: 90000}, async context => {
    const page = await fixture((_request, response) => {
      response.setHeader('Content-Type', 'text/html');
      response.end('<title>Batch fixture</title><button onclick="document.title = `Batch fixture ${++window.count}`">Add</button><script>window.count=0</script>');
    });
    context.after(() => page.close());
    const store = join(page.root, 'recordings');
    const child = spawn(process.env.AAD_TEST_CLI || resolve('target/debug/aad'),
      ['mcp', '--allow-input', '--browser-port', String(page.port)],
      {stdio:['pipe','pipe','inherit'], env:{...process.env, AAD_RECORDINGS_DIR:store, AAD_CONTROL_LEASE_DIR:join(page.root,'leases')}});
    context.after(() => { child.stdin.end(); child.kill(); });
    const lines = createInterface({input:child.stdout})[Symbol.asyncIterator]();
    let calls = 0, bytes = 0;
    async function tool(name, args, expectError = false) {
      const id = ++calls;
      child.stdin.write(JSON.stringify({jsonrpc:'2.0', id, method:'tools/call', params:{name,arguments:args}})+'\n');
      const reply = (await lines.next()).value;
      bytes += Buffer.byteLength(reply);
      const message = JSON.parse(reply);
      assert.equal(message.id,id);
      assert.equal(message.result?.isError,expectError,reply);
      return JSON.parse(message.result.content[0].text);
    }
    const app = {provider:'browser.cdp', url:page.url};
    const locator = {role:'button',name:'Add'};
    const click = {app,locator,action:{type:'click'}};
    const targets = (await tool('control_targets',{})).data.resources;
    const resource_id = targets.find(resource => resource.metadata.url === page.url).resource_id;
    const observe = async () => (await tool('control_observe',{resource_id})).data.observation_id;
    const observation_id = await observe();
    const found = await tool('control_find_many',{observation_id,locators:[locator,{name:'Absent'}],expect:'optional'});
    assert.deepEqual(found.data.results.map(result=>result.found),[true,false]);
    const fields = [{as:'button', locator, property:'name'},
      {as:'missing',locator:{name:'Absent'},property:'present'}];
    const projected = (await tool('control_read_fields',{resource_id,fields})).data;
    assert.deepEqual(projected,{values:{button:'Add',missing:false}});
    const measure = async (label, action) => {
      const beforeCalls=calls, beforeBytes=bytes, start=performance.now();
      await action();
      context.diagnostic(`${label}: ${calls-beforeCalls} MCP calls, ${bytes-beforeBytes} reply bytes, ${Math.round(performance.now()-start)} ms`);
    };
    await measure('interactive 10 clicks',async () => {
      for(let index=0;index<10;index++) {
        const matched=await tool('control_find',{observation_id:await observe(),locator});
        await tool('control_act',{target:matched.data.target,action:click.action});
      }
    });
    assert.equal(await page.cdp.eval('window.count'),10);
    await measure('inline 10 clicks', async () => {
      const result=await tool('run_steps',{steps:Array(10).fill(click)});
      assert.equal(result.status,'succeeded');
      assert.equal(result.executed_steps,10);
    });
    assert.equal(await page.cdp.eval('window.count'),20);
    await tool('run_steps',{steps:[click,{...click,action:{type:'script'}}]},true);
    assert.equal(await page.cdp.eval('window.count'),20);
    const failed=await tool('run_steps',{steps:[click,{...click,locator:{name:'Absent'}},click]},true);
    assert.equal(failed.error.stepId,'s2');
    assert.equal(await page.cdp.eval('window.count'),21);
    assert.deepEqual(await readdir(store).catch(error => { if(error.code==='ENOENT') return []; throw error; }),[]);
    const saved=await tool('save_workflow',{name:'hundred-clicks',steps:Array(100).fill(click)});
    for(const file of [saved.recording_path,saved.path]) {
      const text=await readFile(file,'utf8');
      assert.equal(JSON.parse(text).steps.length,100);
      context.diagnostic(`${file.slice(store.length+1)}: ${text.trimEnd().split('\n').length} lines, ${Buffer.byteLength(text)} bytes`);
    }
  });
