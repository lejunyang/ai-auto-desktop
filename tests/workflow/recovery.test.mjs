import assert from 'node:assert/strict';
import {spawn, execFile} from 'node:child_process';
import {once} from 'node:events';
import {mkdtemp, readFile, rm, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join, resolve} from 'node:path';
import {setTimeout as delay} from 'node:timers/promises';
import {test} from 'node:test';
import {promisify} from 'node:util';

const cli = process.env.AAD_TEST_CLI || resolve('target/debug/aad');
const exec = promisify(execFile);
const plugin = `
import {createInterface} from 'node:readline';
import {appendFileSync} from 'node:fs';
for await (const line of createInterface({input:process.stdin})) {
  const request = JSON.parse(line);
  let result;
  if (request.type === 'manifest') {
    result = {apiVersion:'ai-auto-desktop.dev/v1alpha1', kind:'CapabilityManifest',
      metadata:{name:'ledger'}, actions:{write:{contract_major:1,effect:{default_class:'non_idempotent'}}}};
  } else {
    const order = request.args.order;
    result = {ref:'RCPT-'+order};
    appendFileSync(process.env.ORDERS, JSON.stringify([order,result])+'\\n');
    if (order === 'two') continue;
  }
  console.log(JSON.stringify({id:request.id,result}));
}
`;

for (const mode of ['key', 'ticket']) {
  test('killed after business commit: recover with '+mode, {timeout:30000}, async t => {
    const root = await mkdtemp(join(tmpdir(),'aad-reconcile-'));
    t.after(() => rm(root,{recursive:true,force:true,maxRetries:10,retryDelay:100}));
    const workflow = join(root,'orders.json'), log = join(root,'run.db'), proof = join(root,'receipt.json');
    const service = join(root,'service.mjs'), orders = join(root,'orders.ndjson');
    const env = {...process.env, ORDERS:orders};
    await writeFile(service,plugin);
    await writeFile(workflow,JSON.stringify({apiVersion:'ai-auto-desktop.dev/v1alpha1',kind:'Workflow',
      metadata:{name:'orders'},inputs:{orders:{schema:{type:'array'},required:true}},
      budgets:{max_duration:'30s',max_executed_steps:20},steps:[
        {type:'foreach',id:'batch',items:'${{ inputs.orders }}',as:'order',max_items:3,steps:[
          {id:'submit',type:'action',uses:'ledger.write@1',with:{order:'${{ order }}'}},
          {id:'after',type:'action',uses:'ledger.write@1',with:{order:'${{ steps.submit.output.ref }}'}}]}]}));
    const args = ['run',workflow,'--inputs',JSON.stringify({orders:['one','two','three']}),
      '--plugin',`ledger="${process.execPath}" "${service}"`];
    const start = [...args,'--recovery-log',log,...mode==='key'?['--business-key','submit=/order']:[]];
    const runner = spawn(cli,start,{env,stdio:'ignore'});
    const stopped = once(runner,'exit');
    const run = async args => JSON.parse((await exec(cli,args,{env})).stdout);
    const rows = async () => (await readFile(orders,'utf8')).trim().split('\n').map(line=>JSON.parse(line));
    const resume = [...args,'--recover-from',log,'--receipt',proof];
    try {
      for (let count=0; count<500; count++) {
        if (runner.exitCode!==null) assert.fail('runner exited before its business commit');
        if ((await rows().catch(()=>[])).length === 3) break;
        await delay(20);
      }
      assert.deepEqual((await rows()).map(row=>row[0]),['one','RCPT-one','two']);
      const [key,output] = (await rows()).at(-1);
      await writeFile(proof,JSON.stringify([{step:'submit',key,receipt:output.ref,decision:'applied',output}]));
      await assert.rejects(run(resume), err=>JSON.parse(err.stdout).error.message.includes('locked'));
    } finally {
      runner.kill('SIGKILL');
      await stopped;
    }
    const review = await run(['review',log]);
    assert.equal(review.status,'needs_receipt');
    assert.equal(review.completed_actions,2);
    assert.equal(review.pending.step,'submit');
    await assert.rejects(run([...args,'--recover-from',log]), err=>JSON.parse(err.stdout).error.code==='RECOVERY.REFUSED');
    if (mode === 'ticket') {
      const [,output] = (await rows()).at(-1);
      await writeFile(proof,JSON.stringify({run_id:review.run_id,ticket:review.pending.ticket,step:'submit',decision:'applied',receipt:output.ref,output}));
    }
    assert.equal((await run(resume)).status,'succeeded');
    await assert.rejects(run(resume), err=>JSON.parse(err.stdout).error.code==='RECOVERY.REFUSED');
    const finished = await run(['review',log]);
    assert.equal(finished.status,'succeeded');
    assert.equal(finished.reconciliations[0].receipt,'RCPT-two');
    assert.deepEqual((await rows()).map(row=>row[0]),['one','RCPT-one','two','RCPT-two','three','RCPT-three']);
  });
}
