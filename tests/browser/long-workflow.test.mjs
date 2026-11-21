import assert from 'node:assert/strict';
import {execFile,spawn} from 'node:child_process';
import {readFile, writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {promisify} from 'node:util';
import {test} from 'node:test';
import {setTimeout as delay} from 'node:timers/promises';
import {fixture} from './fixture.mjs';

const exec = promisify(execFile);
test('compact recipes execute 100 and 500 steps; a failed check stops at step 53',
  {skip:!process.env.AAD_TEST_CHROME,timeout:180000}, async context => {
    const page = await fixture((_req,res) => {
      res.setHeader('content-type','text/html');
      res.end('<title>Order audit</title><input aria-label="Order"><input aria-label="Receipt" readonly><button onclick="orders.push(order.value);receipt.value=order.value">Save</button><script>window.orders=[];const [order,receipt]=document.querySelectorAll("input")</script>');
    });
    context.after(() => page.close());
    const env = {...process.env,AAD_CDP_PORT:String(page.port),AAD_CONTROL_LEASE_DIR:join(page.root,'leases')};
    const cli = process.env.AAD_TEST_CLI || resolve('target/debug/aad');
    async function command(args) {
      const {stdout} = await exec(cli,args,{env,maxBuffer:8*1024*1024});
      return JSON.parse(stdout);
    }
    const app = {provider:'browser.cdp',url:page.url};
    for (const [count,fail] of [[100,false],[500,false],[100,true]]) {
      await page.cdp.eval('orders.length=0; order.value=""; receipt.value="";true');
      const expected=[], steps=[];
      for (let index=1; index<=count; index++) {
        const number='ORD-'+Math.ceil(index/4);
        const base={id:'s'+index,app};
        if (fail && index===53) steps.push({...base,read:[{as:'failed_check',locator:{name:'Receipt'},property:'value'}],expect:{failed_check:'missing'},within_ms:100});
        else if (index%4===1) steps.push({...base,locator:{name:'Order'},action:{type:'fill',value:number}});
        else if (index%4===2) {
          steps.push({...base,locator:{name:'Save',role:'button'},action:{type:'click'}});
          if (!fail || index<53) expected.push(number);
        } else if (index%4===3) steps.push({...base,read:[{as:'receipt_'+index,locator:{name:'Receipt'},property:'value'}],expect:{['receipt_'+index]:number}});
        else steps.push({...base,locator:{name:'Save'},check:{present:true}});
      }
      const name='orders'+count+(fail?'fail':'');
      const file=join(page.root,name+'.json'), out=join(page.root,name+'.yaml');
      const original=JSON.stringify({kind:'Recording',format:'control-steps/1',metadata:{name},steps},null,2);
      await writeFile(file,original);
      const exported=await command(['recipe',file,'--out',out]);
      assert.ok(exported.lines < original.split('\n').length/2);
      assert.equal((await command(['validate',out])).steps,count);
      const args=['run',out,'--permission','desktop.input','--permission','desktop.observe','--journal',out+'.jsonl'];
      const started=performance.now();
      if (fail) {
        await assert.rejects(command(args), error => error.code===1 && error.stdout.includes('STEP.CHECK_FAILED'));
        const journal=(await readFile(out+'.jsonl','utf8')).trim().split('\n').map(line=>JSON.parse(line));
        assert.equal(journal.filter(event=>event.type==='step.started').at(-1).payload.id,'s53');
        const review=await command(['review',out+'.jsonl']);
        assert.equal(review.status,'failed');
        assert.equal(review.last_completed_step,'s52');
        assert.equal(review.last_action_error.step,'s53');
        const slowest=journal.filter(event=>event.type==='step.finished')
          .sort((a,b)=>b.payload.elapsed_ms-a.payload.elapsed_ms).slice(0,5)
          .map(({seq,payload})=>({step:payload.id,status:payload.status,elapsed_ms:payload.elapsed_ms,event:seq}));
        assert.deepEqual(review.slowest_steps,slowest);
      } else {
        const result=await command(args);
        assert.equal(result.status,'succeeded');
        assert.equal(Object.keys(result.outputs).length,count/4);
      }
      assert.deepEqual(await page.cdp.eval('orders'),expected);
      context.diagnostic(`${name}: ${exported.lines} recipe lines / ${original.split('\n').length} recording lines; ${expected.length} verified orders; ${Math.round(performance.now()-started)} ms`);
    }
    await page.cdp.eval(`orders.length=0;document.querySelector('button').onclick=()=>{orders.push('ORD-once');debugger;};true`);
    const recipe=join(page.root,'interrupt.yaml'), journal=join(page.root,'interrupt.ndjson');
    const phase={app,steps:[{id:'submit',locator:{role:'button',name:'Save'},action:{type:'click'}},
      {id:'again',locator:{role:'button',name:'Save'},action:{type:'click'}}]};
    await writeFile(recipe,JSON.stringify({kind:'Recipe',name:'interrupt',phases:[phase]}));
    await page.cdp.call('Debugger.enable');
    const runner=spawn(cli,['run',recipe,'--permission','desktop.input','--permission','desktop.observe','--journal',journal],{env,stdio:'ignore'});
    const exited=new Promise((resolve,reject)=>{runner.once('exit',resolve);runner.once('error',reject);});
    try {
      for(let attempt=0;attempt<250&&!page.cdp.events.some(e=>e.method==='Debugger.paused');attempt++) await delay(20);
      assert.ok(page.cdp.events.some(e=>e.method==='Debugger.paused'),'button handler reached its commit');
      runner.kill('SIGKILL');await exited;
    } finally {
      runner.kill();await page.cdp.call('Debugger.disable');
    }
    assert.deepEqual(await page.cdp.eval('orders'),['ORD-once']);
    const review=await command(['review',journal]);
    assert.equal(review.status,'no_final_event');
    assert.deepEqual(review.unconfirmed_action,{step:'submit',uses:'control.session.act@1',effect:'unknown'});
    assert.deepEqual(await page.cdp.eval('orders'),['ORD-once']);
  });
