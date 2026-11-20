import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {test} from 'node:test';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

const exec = promisify(execFile);
const cli = process.env.AAD_TEST_CLI || resolve('target/debug/aad');

async function ordersPage(context) {
  const page = await fixture((_request, response) => {
    response.setHeader('content-type', 'text/html');
    response.end(`<title>Recipe orders</title><input aria-label="Order">
      <button onclick="orders.push(document.querySelector('input').value)">Save</button>
      <script>window.orders = []</script>`);
  });
  context.after(() => page.close());
  const env = {...process.env, AAD_CDP_PORT: String(page.port), AAD_CONTROL_LEASE_DIR: join(page.root, 'leases')};
  return {
    ...page,
    app: {provider: 'browser.cdp', url: page.url},
    async command(...args) { return JSON.parse((await exec(cli, args, {env})).stdout); },
  };
}

const fill = value => ({locator: {name: 'Order'}, action: {type: 'fill', value}});
const save = {locator: {role: 'button', name: 'Save'}, action: {type: 'click'}};
const permissions = ['--permission', 'desktop.input', '--permission', 'desktop.observe'];

test('recipe loops, branches and lexical bindings drive an isolated order form',
  {skip: !process.env.AAD_TEST_CHROME, timeout: 120000}, async context => {
    const page = await ordersPage(context);
    const file = join(page.root, 'program.json');
    const items = Array.from({length: 55}, (_, i) => ({order: 'ORD-' + i, selected: i % 3 !== 0}));
    const program = {
      kind: 'Recipe', format: 'recipe/2', name: 'orders', inputs: ['orders'],
      phases: [{app: page.app, steps: [
        {set: {count: 0, tail: 0}},
        {foreach: '${{ orders }}', as: 'row', index_as: 'i', max_items: items.length, steps: [
          {if: '${{ row.selected }}', then: [fill('${{ row.order }}'), save, {set: {count: '${{ count + 1 }}'}}],
            else: [fill('skip-${{ i }}'), save]},
        ]},
        {while: '${{ tail < 3 }}', max_iterations: 3, steps: [
          fill('tail-${{ tail }}'), save, {set: {tail: '${{ tail + 1 }}'}},
        ]},
      ]}],
      outputs: {selected: '${{ count }}', tail: '${{ tail }}'},
    };
    await writeFile(file, JSON.stringify(program));
    await page.command('validate', file);
    const run = () => page.command('run', file, '--inputs', JSON.stringify({orders: items}), ...permissions);
    const result = await run();
    assert.deepEqual(result.outputs, {selected: items.filter(i => i.selected).length, tail: 3});
    const expected = items.map((row, i) => row.selected ? row.order : 'skip-' + i).concat('tail-0','tail-1','tail-2');
    assert.deepEqual(await page.cdp.eval('orders'), expected);

    // A write before an invalid, untaken branch must never reach the form.
    program.phases[0].steps = [save, {if: '${{ False }}', then: [fill('${{ row.order }}')]}];
    await writeFile(file, JSON.stringify(program));
    await assert.rejects(run, error => error.stdout.includes('unknown recipe name row'));
    assert.deepEqual(await page.cdp.eval('orders'), expected);

    program.outputs = {};
    program.phases[0].steps = [{foreach: '${{ orders }}', as: 'row', max_items: 1, steps: [save]}];
    await writeFile(file, JSON.stringify(program));
    await assert.rejects(run, error => error.stdout.includes('LOOP.MAX_ITEMS_EXCEEDED'));
    assert.deepEqual(await page.cdp.eval('orders'), expected);

    program.phases[0].steps = [{while: '${{ True }}', max_iterations: 2, steps: [fill('limit'), save]}, save];
    await writeFile(file, JSON.stringify(program));
    await assert.rejects(run, error => error.stdout.includes('LOOP.MAX_ITERATIONS_EXCEEDED'));
    assert.deepEqual(await page.cdp.eval('orders'), expected.concat('limit','limit'));
  });
