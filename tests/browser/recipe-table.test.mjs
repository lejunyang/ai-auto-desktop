import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {mkdir, writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {test} from 'node:test';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

const exec = promisify(execFile);

test('collect paginated records, call a pinned subrecipe for each, and reconcile server receipts',
  {skip: !process.env.AAD_TEST_CHROME, timeout: 60000}, async context => {
    const rows = [
      {Order: 'S-04', Amount: '10.00'}, {Order: 'S-08', Amount: '3.50'},
      {Order: 'S-15', Amount: '0.10'}, {Order: 'S-16', Amount: '7.25'},
      {Order: 'S-23', Amount: '2.00'},
    ];
    const receipts = [];
    const page = await fixture((req, res) => {
      const url = new URL(req.url, 'http://localhost');
      if (url.pathname === '/commit' && req.method === 'POST') {
        receipts.push(url.searchParams.get('order'));
        res.end(receipts.at(-1));
      } else if (url.pathname === '/receipts') {
        res.end(JSON.stringify(receipts));
      } else {
        res.setHeader('content-type', 'text/html');
        res.end(`<title>Ledger</title><table aria-label="Orders"><thead><tr><th>Order</th><th>Amount</th></tr></thead><tbody></tbody></table>
          <button id="next">Next</button><input aria-label="Payment"><button id="pay">Submit</button>
          <input aria-label="Receipt" readonly><script>
          const rows = ${JSON.stringify(rows)};
          window.turns = 0;
          function render() {
            document.querySelector('tbody').innerHTML = rows.slice(turns*2, turns*2+2).map(r=>'<tr><td>'+r.Order+'</td><td>'+r.Amount+'</td></tr>').join('');
            next.disabled = (turns+1)*2 >= rows.length;
          }
          next.onclick = () => { ++turns; render(); };
          pay.onclick = async () => {
            const value = document.querySelector('[aria-label=Payment]').value;
            const reply = await fetch('/commit?order='+encodeURIComponent(value), {method:'POST'});
            document.querySelector('[aria-label=Receipt]').value = await reply.text();
          };
          render();</script>`);
      }
    });
    context.after(() => page.close());
    const env = {...process.env, AAD_CDP_PORT: String(page.port), AAD_CONTROL_LEASE_DIR: join(page.root, 'leases')};
    const cli = process.env.AAD_TEST_CLI || resolve('target/debug/aad');
    const app = {provider: 'browser.cdp', url: page.url};
    const file = join(page.root, 'ledger.yaml');
    const lib = join(page.root, 'lib', 'submit');
    await mkdir(lib, {recursive: true});
    await writeFile(join(lib, '1.0.0.yaml'), JSON.stringify({
      kind: 'Subrecipe', format: 'recipe/2', name: 'submit', version: '1.0.0', parameters: ['row'],
      phases: [{app, steps: [
        {set: {payment: '${{ row.Order }}=${{ row.Amount }}'}},
        {locator: {name: 'Payment'}, action: {type: 'fill', value: '${{ payment }}'}},
        {locator: {name: 'Submit'}, action: {type: 'click'}},
        {id: 'seen', read: [{as: 'receipt', locator: {name: 'Receipt'}, property: 'value'}],
          expect: {receipt: '${{ payment }}'}, within_ms: 3000},
      ]}], results: {receipt: '${{ seen.values.receipt }}'},
    }));
    const collection = {table: 'ledger', columns: ['Order','Amount'], row_key: 'Order', expected_rows: 5,
      max_pages: 3, traversal: {mode: 'pagination', next: 'advance'}};
    const program = {
      kind: 'Recipe', format: 'recipe/2', name: 'settle',
      targets: {ledger: {role: 'table', name: 'Orders'}, advance: {role: 'button', name: 'Next'}},
      phases: [{app, steps: [
        {id: 'orders', collect_table: collection},
        {set: {receipts: []}},
        {if: '${{ orders.complete }}', then: [{foreach: '${{ orders.records }}', as: 'row', max_items: 5,
          steps: [{call: 'submit@1.0.0', with: {row: '${{ row }}'}, save: {receipt: 'ref'}},
            {set: {receipts: '${{ receipts + [ref] }}'}}]}]},
      ]}], outputs: {complete: '${{ orders.complete }}', rows: '${{ orders.records }}', receipts: '${{ receipts }}'},
    };
    const command = async args => JSON.parse((await exec(cli, args, {env})).stdout);
    async function run() {
      await writeFile(file, JSON.stringify(program));
      return command(['run', file, '--permission', 'desktop.observe', '--permission', 'desktop.input']);
    }
    const committed = async () => (await fetch(page.url + 'receipts')).json();
    collection.max_pages = 1;
    const partial = await run();
    assert.equal(partial.outputs.complete, false);
    assert.deepEqual(await committed(), []);
    assert.equal(await page.cdp.eval('turns'), 0);
    collection.max_pages = 3;
    const result = await run();
    const expected = rows.map(r => r.Order + '=' + r.Amount);
    assert.deepEqual(result.outputs, {complete: true, rows, receipts: expected});
    assert.deepEqual(await committed(), expected);
    assert.equal(await page.cdp.eval('turns'), 2);

    // Even a trailing, unreachable collection is validated before earlier actions.
    program.outputs = {};
    for (const invalid of [{row_key:'Missing'}, {max_rows:0}, {columns:['Order','Order']},
      {table:{node_id:'stale'}}, {traversal:{mode:'pagination',next:'unknown'}}]) {
      program.phases[0].steps = [{locator:{name:'Submit'},action:{type:'click'}},
        {if:'${{ False }}',then:[{collect_table:{...collection,...invalid}}]}];
      await assert.rejects(run, error => error.stdout.includes('CLI.RECIPE_INVALID'));
      assert.deepEqual(await committed(), expected);
    }
  });
