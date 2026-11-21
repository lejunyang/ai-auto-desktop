// Run with the branch build, AAD_TEST_CHROME and the real gate environment.
import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

const {OMEM_URL, OMEM_TOKEN, AAD_TEST_CHROME} = process.env;
assert.ok(OMEM_URL && OMEM_TOKEN && AAD_TEST_CHROME, 'Set OMEM_URL, OMEM_TOKEN and AAD_TEST_CHROME');
const endpoint = new URL(OMEM_URL.replace(/\/$/,'')+'/api/operation-gate/http');
assert.ok(endpoint.protocol === 'http:' && ['127.0.0.1','localhost','[::1]'].includes(endpoint.hostname));
const scope = await fetch(endpoint, {headers:{authorization:'Bearer '+OMEM_TOKEN}, redirect:'error'});
assert.equal(scope.status, 200);
const {revision, watch} = await scope.json();
console.log('Scope GET', scope.status, JSON.stringify({revision, watch}));
assert.ok(watch.some(rule => rule.host === '127.0.0.1' && rule.methods?.includes('POST')));

const routes = {Deny:['POST','/gate-e2e/deny/thing'], Allow:['POST','/gate-e2e/allow/thing'],
  Read:['GET','/gate-e2e/read']};
const arrivals = [];
const page = await fixture((req,res) => {
  arrivals.push([req.method,req.url]);
  if (req.url !== '/') return res.writeHead(200).end('ok');
  res.setHeader('Content-Type','text/html');
  res.end(Object.entries(routes).map(([name,[method,path]]) =>
    `<button onclick="fetch('${path}',{method:'${method}'}).catch(()=>{})">${name}</button>`).join(''));
});
try {
  const env = {...process.env, HOME:page.root, LOCALAPPDATA:page.root, AAD_CONTROL_LEASE_DIR:join(page.root,'leases')};
  delete env.AAD_REQUEST_GATE_URL;
  delete env.AAD_REQUEST_GATE_TOKEN;
  const file = join(page.root,'click.js');
  async function click(name, token = OMEM_TOKEN) {
    await writeFile(file, `page.click({role:"button",name:${JSON.stringify(name)}}); return true;`);
    const args = ['browser-script', file, '--url',page.url,'--browser-port',String(page.port),'--allow-input'];
    let stdout;
    try {
      ({stdout} = await promisify(execFile)(resolve('target/debug/aad'), args, {env:{...env,OMEM_TOKEN:token}}));
    } catch (error) {
      if (!error.stdout) throw error;
      stdout = error.stdout;
    }
    const report = JSON.parse(stdout);
    const block = report.error?.details?.blocked_requests?.[0];
    const label = token === OMEM_TOKEN ? name : name+' (invalid token)';
    if (block) {
      const {method,url,decision,message,hint} = block;
      const result = JSON.stringify({code:report.error.code,method,path:new URL(url).pathname,decision,message,hint});
      assert.ok(!result.includes(OMEM_TOKEN) && !result.includes(OMEM_URL));
      console.log(label, result);
      assert.equal(method, routes[name][0]);
      assert.equal(url, new URL(routes[name][1],page.url).href);
    } else {
      console.log(label, JSON.stringify({result:report.result}));
      assert.equal(report.result, true);
    }
    return block;
  }
  const denied = await click('Deny');
  assert.equal(denied.decision,'deny');
  assert.ok(denied.message);
  assert.equal(await click('Allow'), undefined);
  assert.equal(await click('Read'), undefined);
  const invalid = await click('Allow','invalid-verification-token');
  assert.equal(invalid.decision,'unavailable');
  assert.match(invalid.message,/token/i);
  assert.deepEqual(arrivals.filter(([,path])=>path.startsWith('/gate-e2e/')), [routes.Allow,routes.Read]);
} finally {
  console.log('Server arrivals',JSON.stringify(arrivals));
  await page.close();
}
