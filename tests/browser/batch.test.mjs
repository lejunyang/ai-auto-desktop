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
    await page.cdp.eval(`document.body.innerHTML = '<input aria-label="Quoted total" value="118.50"><input aria-label="Billed total"><input aria-label="Order ID"><input aria-label="PIN" type="password" value="sample-private"><button>Place order</button>';
      window.orders = 0;
      document.querySelector('button').onclick = () => { ++window.orders; setTimeout(() => {
        document.querySelector('[aria-label="Billed total"]').value = '118.50';
        document.querySelector('[aria-label="Order ID"]').value = 'ORD-42';
      }, 130); }; true`);
    const field = (alias, name) => ({as:alias, locator:{name}, property:'value'});
    const quote = {id:'quote',app,read:[field('quoted','Quoted total')]};
    const submit = {app,locator:{name:'Place order',role:'button'},action:{type:'click'}};
    const receipt = {id:'receipt',app,read:[field('billed','Billed total'),field('order_id','Order ID')],
      expect:{billed:'${{ steps.quote.output.values.quoted }}',order_id:'ORD-42'},within_ms:3000};
    const steps = [quote,submit,receipt];
    const expected = {quoted:'118.50',billed:'118.50',order_id:'ORD-42'};
    assert.deepEqual((await tool('run_steps',{steps})).outputs, expected);
    assert.deepEqual(await page.cdp.eval('[window.orders, document.querySelector(\'[aria-label="Order ID"]\').value]'),[1,'ORD-42']);
    // A busy renderer needs time to complete at least one receipt read.
    const mismatch = {...receipt,expect:{billed:'120.00'},within_ms:1000};
    const stopped = await tool('run_steps',{steps:[quote,mismatch,submit]},true);
    assert.equal(stopped.error.code,'STEP.CHECK_FAILED',JSON.stringify(stopped.error));
    assert.equal(stopped.error.stepId,'receipt');
    assert.equal(stopped.last_completed_step,'quote');
    assert.equal(stopped.next_unstarted_step,'s3');
    assert.equal(stopped.steps.length,2);
    assert.ok(stopped.steps[1].elapsed_ms>=mismatch.within_ms-2);
    assert.ok(stopped.elapsed_ms>=stopped.steps[1].elapsed_ms);
    assert.match(stopped.error.message,/billed/);
    assert.equal(await page.cdp.eval('window.orders'),1);
    const invalid = await tool('run_steps',{steps:[submit,{...quote,expect:{typo:42}}]},true);
    assert.equal(invalid.code,'STEP.INVALID');
    assert.equal(await page.cdp.eval('window.orders'),1);
    const secret = await tool('control_read_fields',{resource_id,fields:[field('pin','PIN')]},true);
    assert.equal(secret.error.code,'POLICY.DENIED');
    assert.ok(!JSON.stringify(secret).includes('sample-private'));
    await tool('save_workflow',{name:'verify-receipt',steps});
    assert.deepEqual((await tool('run_workflow',{name:'verify-receipt'})).outputs,expected);
    assert.equal(await page.cdp.eval('window.orders'),2);
    await page.cdp.eval(`document.querySelector('[aria-label="Order ID"]').value = 'x'.repeat(4100); true`);
    const long = await tool('control_read_fields',{resource_id,fields:[field('order_id','Order ID')]},true);
    assert.equal(long.error.code,'CONTROL.FIELD_TOO_LARGE');
    await page.cdp.eval(`document.body.innerHTML='<table aria-label="Orders" aria-rowcount="1000"><tr></tr></table>';
      const row=document.querySelector('tr');for(let column=0;column<51;column++)row.insertCell().innerText=column===0?'a'.repeat(501):'B';
      const hidden=row.parentElement.insertRow();hidden.hidden=true;hidden.insertCell().innerText='hidden order';true`);
    const view=(await tool('control_observe',{resource_id,view:{mode:'text',max_characters:540}})).data;
    const table=view.tables[0];
    assert.equal(view.tables_truncated,true);
    assert.equal(table.scope,'rendered_rows');
    assert.equal(table.row_count,1);
    assert.deepEqual(new Set(table.truncation_reasons),new Set(['rows_not_rendered','column_limit','cell_limit','character_budget']));
    assert.equal(table.rows[0][0].length,500);
    assert.equal(table.rows[0].length,41);
    await page.cdp.eval(`document.body.innerHTML='<table aria-label="Orders" aria-rowcount="500"><tr><th>ID</th><th>Total</th></tr><tr><td>ORD-1</td><td>118.50</td></tr><tr><td hidden>internal</td><td>2.00</td></tr></table>';true`);
    async function readTable(columns,expectError=false) {
      const found=await tool('control_find',{observation_id:await observe(),locator:{role:'table',name:'Orders'}});
      return tool('control_read_table',{target:found.data.target,columns},expectError);
    }
    const records=(await readTable(['Total','ID'])).data;
    assert.deepEqual(records.records,[{Total:'118.50',ID:'ORD-1'},{Total:'2.00',ID:null}]);
    assert.deepEqual(records.truncation_reasons,['rows_not_rendered']);
    assert.equal(records.visible_data_rows,2);
    await page.cdp.eval("document.querySelector('th').textContent='Total';true");
    const duplicate=await readTable(['Total'],true);
    assert.equal(duplicate.error.code,'CONTROL.TABLE_HEADER');
  });
