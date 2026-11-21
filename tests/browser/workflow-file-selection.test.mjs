import assert from 'node:assert/strict';
import {execFile, spawn} from 'node:child_process';
import {once} from 'node:events';
import {mkdir, readFile, symlink, writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {createInterface} from 'node:readline';
import {test} from 'node:test';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

const cli = process.env.AAD_TEST_CLI || resolve('target/debug/aad');
const exec = promisify(execFile);

async function withServer(port, env, grants, run) {
  const child = spawn(cli, ['mcp', '--browser-port', String(port), '--allow-input', ...grants],
    {env, stdio: ['pipe', 'pipe', 'inherit']});
  const closed = once(child, 'close');
  const lines = createInterface({input: child.stdout})[Symbol.asyncIterator]();
  let id = 0;
  try {
    await run(async (name, args) => {
      child.stdin.write(JSON.stringify({jsonrpc: '2.0', id: ++id, method: 'tools/call', params: {name, arguments: args}}) + '\n');
      const message = JSON.parse((await lines.next()).value);
      assert.equal(message.id, id);
      return JSON.parse(message.result.content[0].text);
    });
  } finally {
    child.stdin.end();
    await closed;
  }
}

test('saved file inputs upload only run-supplied files within CLI or MCP server grants',
  {skip: !process.env.AAD_TEST_CHROME, timeout: 60000}, async context => {
    const received = [];
    const page = await fixture(async (request, response) => {
      if (request.method === 'POST') {
        const body = [];
        for await (const chunk of request) body.push(chunk);
        received.push([decodeURIComponent(request.url.slice(1)), Buffer.concat(body).toString()]);
        response.end();
        return;
      }
      response.setHeader('Content-Type', 'text/html');
      response.end(`<input type="file" aria-label="Attachments" multiple><script>
        document.querySelector('input').onchange = event => window.sent = (async () => {
          for (const file of event.target.files) await fetch('/' + encodeURIComponent(file.name), {method:'POST', body:file});
        })();</script>`);
    });
    context.after(() => page.close());
    const env = {...process.env, AAD_CDP_PORT: String(page.port), AAD_RECORDINGS_DIR: join(page.root, 'store'),
      AAD_CONTROL_LEASE_DIR: join(page.root, 'leases')};
    const firstRoot = join(page.root, 'inbox'), secondRoot = join(page.root, 'more');
    await Promise.all([mkdir(firstRoot), mkdir(secondRoot)]);
    const first = join(firstRoot, 'first.txt'), second = join(secondRoot, 'second.txt');
    await Promise.all([writeFile(first, 'first payload'), writeFile(second, 'second payload')]);
    const inputs = files => ({rec1_files: files});
    await withServer(page.port, env, [], async tool => {
      for (const type of ['upload', 'choose_files']) {
        const result = await tool('save_workflow', {name: type, steps: [{id: 'rec1', app: {provider:'browser.cdp', url:page.url},
          locator: {name:'Attachments'}, action: {type, files:[first]}, input: {name:'rec1_files', sensitive:false}}]});
        assert.deepEqual(result.inputs_required, ['rec1_files'], JSON.stringify(result));
        for (const path of [result.recording_path, result.path]) assert.ok(!(await readFile(path, 'utf8')).includes('first.txt'));
        const described = await tool('describe_workflow', {name:type});
        assert.deepEqual(described.inputs.rec1_files.schema, {type:'array', items:{type:'string'}});
      }
      const args = {name:'upload', inputs:inputs([first])};
      assert.equal((await tool('run_workflow', args)).error.code, 'POLICY.DENIED');
      assert.equal((await tool('run_workflow', {...args, upload_dirs:[firstRoot]})).code, 'MCP.INVALID_ARGUMENT');
    });
    const run = (files, ...flags) => exec(cli, ['run', 'upload', '--inputs', JSON.stringify(inputs(files)),
      '--permission', 'desktop.observe', ...flags], {env});
    const input = ['--permission', 'desktop.input'];
    const root = ['--allow-upload-dir', firstRoot];
    const denied = error => JSON.parse(error.stdout).error.code === 'POLICY.DENIED';
    await assert.rejects(run([first], ...root), denied);
    await assert.rejects(run([first], ...input), denied);
    await assert.rejects(run([first, second], ...input, ...root), denied);
    if (process.platform !== 'win32') {
      const link = join(firstRoot, 'outside-link');
      await symlink(second, link);
      await assert.rejects(run([link], ...input, ...root), denied);
    }
    assert.deepEqual(received, []);
    assert.equal(await page.cdp.eval('document.querySelector("input").files.length'), 0);

    const result = await run([first, second], ...input, ...root, '--allow-upload-dir', secondRoot);
    assert.equal(JSON.parse(result.stdout).status, 'succeeded');
    await page.cdp.eval('window.sent');
    assert.deepEqual(received, [['first.txt','first payload'], ['second.txt','second payload']]);
    await withServer(page.port, env, root, async tool => {
      const replay = files => tool('run_workflow', {name:'choose_files', inputs:inputs(files)});
      assert.equal((await replay([second])).error.code, 'POLICY.DENIED');
      assert.equal((await replay([first])).status, 'succeeded');
      await page.cdp.eval('window.sent');
      assert.deepEqual(received.slice(2), [['first.txt','first payload']]);
    });
  });
