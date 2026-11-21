import assert from 'node:assert/strict';
import {execFile,spawn} from 'node:child_process';
import {access, mkdir, readFile, symlink, unlink, writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {createInterface} from 'node:readline';
import {test} from 'node:test';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

test('Chromium selects only the granted snapshot and submits native directory paths',
  {skip: process.platform !== 'linux' || !process.env.AAD_TEST_CHROME, timeout:60000}, async context => {
    const bodies = [];
    const page = await fixture(async (req,res) => {
      if (req.method === 'POST') {
        const parts=[];
        for await (const part of req) parts.push(part);
        bodies.push(Buffer.concat(parts).toString());
      } else {
        res.setHeader('content-type','text/html');
        res.write('<form><input type=file webkitdirectory name=tree aria-label="Project"></form><script>window.changed=0;document.querySelector("input").onchange=()=>changed++</script>');
      }
      res.end();
    });
    context.after(()=>page.close());
    const tree=join(page.root,'bundle'), nested=join(tree,'assets','nested');
    const stage=join(page.root,'private-snapshot'), other=join(page.root,'outside');
    await mkdir(nested,{recursive:true}); await mkdir(other);
    await writeFile(join(tree,'notes.txt'),'root payload');
    await writeFile(join(nested,'notes.txt'),'nested payload');
    await writeFile(join(other,'secret.txt'),'outside payload');
    const script=join(page.root,'choose.js');
    await writeFile(script,'return page.upload({name:"Project"}, [inputs.tree]);');
    const cli=process.env.AAD_TEST_CLI || resolve('target/debug/aad');
    const run=(path,...flags)=>promisify(execFile)(cli,['browser','script',script,'--url',page.url,'--browser-port',String(page.port),
      '--allow-input','--inputs',JSON.stringify({tree:path}),...flags],{env:{...process.env,AAD_CONTROL_LEASE_DIR:join(page.root,'leases')}});
    const grant=['--allow-upload-tree',tree,'--upload-tree-stage',stage];
    const denied=err=>JSON.parse(err.stdout).error.code==='POLICY.DENIED';
    await assert.rejects(run(tree,'--allow-upload-dir',tree),denied);
    await assert.rejects(run(nested,...grant),denied);
    for (const target of [other,join(other,'secret.txt')]) {
      await symlink(target,join(nested,'external'));
      await assert.rejects(run(tree,...grant),denied);
      await assert.rejects(access(stage),{code:'ENOENT'});
      await unlink(join(nested,'external'));
    }
    assert.equal(await page.cdp.eval('changed'),0);
    const result=JSON.parse((await run(tree,...grant)).stdout);
    assert.equal(result.completed_actions,1);
    await writeFile(join(nested,'notes.txt'),'changed after selection');
    assert.equal(await readFile(join(stage,'bundle','assets','nested','notes.txt'),'utf8'),'nested payload');
    assert.equal(await page.cdp.eval('changed'),1);
    const data=await page.cdp.eval(`Promise.all(Array.from(document.querySelector('input').files,async f=>[f.webkitRelativePath,await f.text()]))`);
    assert.deepEqual(data.sort(),[['bundle/assets/nested/notes.txt','nested payload'],['bundle/notes.txt','root payload']]);
    await page.cdp.eval('fetch("/",{method:"POST",body:new FormData(document.forms[0])}).then(r=>r.ok)');
    assert.equal(bodies.length,1);
    for (const [path,contents] of data) {
      assert.ok(bodies[0].includes('filename="'+path+'"'));
      assert.ok(bodies[0].includes('\r\n\r\n'+contents+'\r\n'));
    }
    assert.ok(!bodies[0].includes('outside payload'));
  });

test('switching an observed file input to directory mode requires a fresh observation', {
  skip: !process.env.AAD_TEST_CHROME, timeout:30000
}, async context => {
  const page=await fixture((_,res)=>{
    res.setHeader('content-type','text/html');
    res.end('<input type=file aria-label="Attachments">');
  });
  context.after(()=>page.close());
  const file=join(page.root,'invoice.txt');
  await writeFile(file,'only this file');
  const child=spawn(process.env.AAD_TEST_CLI || resolve('target/debug/aad'),
    ['mcp','--allow-input','--allow-upload-dir',page.root,'--browser-port',String(page.port),'--provider','browser.cdp'],
    {stdio:['pipe','pipe','inherit'],env:{...process.env,AAD_CONTROL_LEASE_DIR:join(page.root,'leases')}});
  context.after(()=>{child.stdin.end();child.kill();});
  const lines=createInterface({input:child.stdout})[Symbol.asyncIterator]();
  let id=0;
  async function tool(name,args,error=false) {
    child.stdin.write(JSON.stringify({jsonrpc:'2.0',id:++id,method:'tools/call',params:{name,arguments:args}})+'\n');
    const reply=JSON.parse((await lines.next()).value).result;
    assert.equal(reply.isError,error,JSON.stringify(reply));
    return JSON.parse(reply.content[0].text);
  }
  const {resources}=(await tool('control_targets',{})).data;
  const resource_id=resources.find(row=>row.metadata.url===page.url).resource_id;
  const {observation_id}=(await tool('control_observe',{resource_id})).data;
  const {target}=(await tool('control_find',{observation_id,locator:{name:'Attachments'}})).data;
  await page.cdp.eval('document.querySelector("input").webkitdirectory=true');
  const refused=await tool('control_act',{target,action:{type:'upload',files:[file]}},true);
  assert.equal(refused.error.code,'CONTROL.STALE_TARGET');
  assert.equal(await page.cdp.eval('document.querySelector("input").files.length'),0);
});
