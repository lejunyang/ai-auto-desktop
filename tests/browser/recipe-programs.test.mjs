import assert from 'node:assert/strict';
import {execFile, spawn} from 'node:child_process';
import {mkdir, readFile, writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {createInterface} from 'node:readline';
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

test('version-pinned nested subrecipes isolate arguments, reads and return slots on every call',
  {skip: !process.env.AAD_TEST_CHROME, timeout: 60000}, async context => {
    const page = await ordersPage(context);
    const file = join(page.root, 'orders.yaml');
    const directory = join(page.root, 'library');
    await mkdir(directory);
    const pin = (file, name) => ({file, name, version: '1.2.0'});
    const innerPin = pin('submit.json', 'submit');
    const outerPin = pin('library/pair.yaml', 'pair');
    const inner = {
      kind: 'Subrecipe', format: 'recipe/2', name: 'submit', version: '1.2.0', parameters: ['order'],
      phases: [{app: page.app, steps: [fill('${{ order }}'), save,
        {id: 'receipt', read: [{as: 'actual', locator: {name: 'Order'}, property: 'value'}]}]}],
      results: {number: '${{ receipt.values.actual }}'},
    };
    await writeFile(join(directory, innerPin.file), JSON.stringify(inner));
    const outer = {
      kind: 'Subrecipe', format: 'recipe/2', name: 'pair', version: '1.2.0', parameters: ['order'],
      phases: [{app: page.app, steps: [
        {call: innerPin, with: {order: '${{ order }}'}, save: {number: 'receipt'}},
        {call: innerPin, with: {order: '${{ receipt }}-copy'}, save: {number: 'copy'}},
      ]}], results: {pair: ['${{ receipt }}', '${{ copy }}']},
    };
    await writeFile(join(page.root, outerPin.file), JSON.stringify(outer));
    const call = {call: outerPin, with: {order: '${{ order }}'}, save: {pair: 'last'}};
    const root = {
      kind: 'Recipe', format: 'recipe/2', name: 'caller',
      phases: [{app: page.app, steps: [
        {set: {last: [], receipt: 'caller', order: 'outside', pairs: []}},
        {foreach: "${{ ['A','B','C'] }}", as: 'order', max_items: 3, steps: [
          call, {set: {pairs: '${{ pairs + [last] }}'}},
        ]},
      ]}], outputs: {pairs: '${{ pairs }}', receipt: '${{ receipt }}', order: '${{ order }}'},
    };
    await writeFile(file, JSON.stringify(root));
    await page.command('validate', file);
    const run = () => page.command('run', file, ...permissions);
    const result = await run();
    const pairs = ['A','B','C'].map(order => [order, order + '-copy']);
    assert.deepEqual(result.outputs, {pairs, receipt: 'caller', order: 'outside'});
    assert.deepEqual(await page.cdp.eval('orders'), pairs.flat());

    // Each mutation is behind a prior click and inside an untaken branch.
    root.outputs = {};
    const invoke = {call: outerPin, with: {order: 'X'}};
    const invalid = async (statement, pattern) => {
      root.phases[0].steps = [save, {if: '${{ False }}', then: [statement]}];
      await writeFile(file, JSON.stringify(root));
      await assert.rejects(run, error => error.stdout.includes('CLI.RECIPE_INVALID') && pattern.test(error.stdout));
      assert.deepEqual(await page.cdp.eval('orders'), pairs.flat());
    };
    await invalid({...invoke, call: {...outerPin, version: '1.3.0'}}, /version 1.3.0/);
    await invalid({...invoke, call: {...outerPin, name: 'other'}}, /named other/);
    await invalid({...invoke, with: {}}, /arguments/);
    await invalid({...invoke, with: {order: 'X', extra: 1}}, /arguments/);
    await invalid({...invoke, save: {missing: 'capture'}}, /unknown subrecipe result/);
    outer.results = {bad: '${{ last }}'};
    await writeFile(join(page.root, outerPin.file), JSON.stringify(outer));
    await invalid(invoke, /unknown recipe name last/);
    outer.phases[0].steps = [{call: pin('pair.yaml', 'pair'), with: {order: 'X'}}];
    await writeFile(join(page.root, outerPin.file), JSON.stringify(outer));
    await invalid(invoke, /recursive subrecipe/);
    await writeFile(join(page.root, outerPin.file), '{"kind":"Subrecipe","kind":"Recipe"}');
    await invalid(invoke, /duplicate/);
  });

test('MCP runs the locally published recipe even after the callable source changes',
  {skip: !process.env.AAD_TEST_CHROME, timeout: 60000}, async context => {
    const page = await ordersPage(context);
    const store = join(page.root, 'recordings');
    await mkdir(store);
    const name = 'approved-orders';
    const file = join(page.root, 'approved.yaml');
    const library = join(page.root, 'save.json');
    const artifact = join(store, name + '.workflow.json');
    const spec = {kind: 'Subrecipe', format: 'recipe/2', name: 'save', version: '1.0.0',
      parameters: ['number'], phases: [{app: page.app, steps: [fill('${{ number }}'), save]}]};
    await writeFile(library, JSON.stringify(spec));
    await writeFile(file, JSON.stringify({kind: 'Recipe', format: 'recipe/2', name, inputs: ['number'],
      phases: [{app: page.app, steps: [{call: {name: 'save', version: '1.0.0', file: 'save.json'},
        with: {number: '${{ number }}'}}]}], outputs: {receipt: '${{ number }}'}}));
    await page.command('recipe', file, '--out', artifact);
    const published = await readFile(artifact, 'utf8');
    assert.equal(JSON.parse(published).kind, 'Workflow');
    await assert.rejects(page.command('recipe', file, '--out', artifact));
    await writeFile(library, '{}');
    await assert.rejects(page.command('recipe', file, '--out', join(store, 'bad.workflow.json')));
    assert.equal(await readFile(artifact, 'utf8'), published);
    await assert.rejects(readFile(join(store, 'bad.workflow.json')), {code: 'ENOENT'});
    assert.deepEqual(await page.cdp.eval('orders'), []);

    const child = spawn(cli, ['mcp', '--allow-input', '--browser-port', String(page.port)], {
      stdio: ['pipe', 'pipe', 'inherit'],
      env: {...process.env, AAD_RECORDINGS_DIR: store, AAD_CONTROL_LEASE_DIR: join(page.root, 'leases')},
    });
    context.after(() => { child.stdin.end(); child.kill(); });
    const lines = createInterface({input: child.stdout})[Symbol.asyncIterator]();
    let id = 0;
    async function tool(name, args) {
      child.stdin.write(JSON.stringify({jsonrpc: '2.0', id: ++id, method: 'tools/call', params: {name, arguments: args}}) + '\n');
      const message = JSON.parse((await lines.next()).value);
      assert.equal(message.id, id);
      assert.equal(message.result.isError, false, JSON.stringify(message));
      return JSON.parse(message.result.content[0].text);
    }
    const description = await tool('describe_workflow', {name});
    assert.equal(description.runnable, true);
    assert.equal(description.inputs.number.required, true);
    const result = await tool('run_workflow', {name, inputs: {number: 'MCP-42'}});
    assert.equal(result.status, 'succeeded');
    assert.deepEqual(result.outputs, {receipt: 'MCP-42'});
    assert.deepEqual(await page.cdp.eval('orders'), ['MCP-42']);
  });
