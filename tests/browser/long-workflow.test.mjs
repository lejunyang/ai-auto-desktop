import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {readFile, writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {promisify} from 'node:util';
import {test} from 'node:test';
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
      } else {
        const result=await command(args);
        assert.equal(result.status,'succeeded');
        assert.equal(Object.keys(result.outputs).length,count/4);
      }
      assert.deepEqual(await page.cdp.eval('orders'),expected);
      context.diagnostic(`${name}: ${exported.lines} recipe lines / ${original.split('\n').length} recording lines; ${expected.length} verified orders; ${Math.round(performance.now()-started)} ms`);
    }
  });
