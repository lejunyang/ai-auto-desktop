import assert from 'node:assert/strict';
import {execFile, spawn} from 'node:child_process';
import {readFile, writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {createInterface} from 'node:readline';
import {test} from 'node:test';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

const cli = process.env.AAD_TEST_CLI || resolve('target/debug/aad');
async function openTable(t, html, role = 'table') {
  const page = await fixture((_req, res) => {
    res.setHeader('Content-Type','text/html;charset=utf-8');
    res.end(html);
  });
  t.after(() => page.close());
  const child = spawn(cli, ['mcp', '--browser-port', String(page.port)], {stdio:['pipe','pipe','inherit']});
  t.after(() => { child.stdin.end(); child.kill(); });
  const lines = createInterface({input:child.stdout})[Symbol.asyncIterator]();
  let id = 0;
  async function tool(name, args, error) {
    child.stdin.write(JSON.stringify({jsonrpc:'2.0',id:++id,method:'tools/call',params:{name,arguments:args}})+'\n');
    const reply = JSON.parse((await lines.next()).value).result;
    const result = JSON.parse(reply.content[0].text);
    if (error) assert.equal(result.error?.code, error, JSON.stringify(result));
    else assert.equal(reply.isError, false, JSON.stringify(result));
    return result.data;
  }
  const resources = (await tool('control_targets', {})).resources;
  const resource_id = resources.find(r => r.metadata.url === page.url).resource_id;
  const locator = {role, name:'Ledger'};
  async function read(columns, error, options = {}) {
    const {observation_id} = await tool('control_observe', {resource_id});
    const {target} = await tool('control_find', {observation_id, locator});
    return tool('control_read_table', {target, columns, ...options}, error);
  }
  async function collect(columns, expected) {
    const spec = {table:locator, columns, row_key:'ID', expected_rows:expected,
      traversal:{mode:'pagination', next:{role:'button',name:'Next'}}};
    await writeFile(join(page.root,'spec.json'), JSON.stringify(spec));
    const {stdout} = await promisify(execFile)(cli, ['browser','collect-table','--browser-port',String(page.port),
      '--url',page.url,'--allow-input','--spec','spec.json','--output','actual.csv'], {cwd:page.root});
    const result = JSON.parse(stdout);
    assert.equal(result.complete, true);
    return {records:result.records, csv:await readFile(join(page.root,'actual.csv'),'utf8')};
  }
  return {page, read, collect};
}

test('HTML table spans preserve header paths and CSV order within the output budget',
  {skip:!process.env.AAD_TEST_CHROME}, async t => {
    const {page, read, collect} = await openTable(t, `
      <table aria-label="Ledger" aria-rowcount="6">
      <thead><tr><th rowspan="3">ID</th><th colspan="2">Q/1</th><th colspan="2">Q~2</th></tr>
        <tr><th rowspan="2">Units</th><th>Net</th><th colspan="2">Stats</th></tr>
        <tr><th>Net</th><th>Units</th><th>Net</th></tr></thead>
      <tbody><tr><td>A</td><td rowspan="0">7</td><td colspan="2">x</td><td>一,二</td></tr>
        <tr hidden><td>hidden</td></tr><tr><td>B</td><td>2</td><td>3</td><td>4</td></tr></tbody>
      <tbody><tr><td>C</td><td>8</td><td>9</td><td>10</td><td>11</td></tr></tbody></table>`);
    const columns = ['ID','/Q~11/Units','/Q~11/Net/Net','/Q~02/Stats/Units','/Q~02/Stats/Net'];
    const expected = [['A','7','x','x','一,二'],['B','7','2','3','4'],['C','8','9','10','11']]
      .map(row => Object.fromEntries(columns.map((key,i) => [key,row[i]])));
    const result = await read(columns);
    assert.equal(result.visible_data_rows, 3);
    assert.equal(result.truncated, false);
    assert.deepEqual(result.records, expected);
    await read(['Net'], 'CONTROL.TABLE_HEADER');
    const all = await collect(columns, 3);
    assert.deepEqual(all.records, expected);
    assert.equal(all.csv, columns.join(',')+'\r\nA,7,x,x,"一,二"\r\nB,7,2,3,4\r\nC,8,9,10,11\r\n');

    const set = html => page.cdp.eval(`document.querySelector('table').innerHTML = ${JSON.stringify(html)}; true`);
    await set('<tr><th>Key</th><th>Data</th></tr><tr><td>A</td><td rowspan=2>taken</td></tr><tr><td colspan=2>overlap</td></tr>');
    await read(['Key'], 'CONTROL.TABLE_HEADER');
    await set('<tr><th colspan=2>Group</th></tr><tr><th>Price</th><th>Price</th></tr>');
    await read(['/Group/Price'], 'CONTROL.TABLE_HEADER');
    await set('<tr><th>/literal</th><th>Data</th></tr><tr><td colspan=1000 rowspan=65534>'+'x'.repeat(499)+'</td></tr>');
    const partial = await read(['/literal','Data'], null, {max_characters:500});
    assert.equal(partial.truncated, true);
    assert.ok(partial.truncation_reasons.includes('character_budget'));
    assert.ok(partial.truncation_reasons.includes('column_limit'));
    assert.equal(partial.records[0]['/literal'].length, 500-'/literalData'.length);
    assert.equal(partial.records[0].Data, null);
  });
