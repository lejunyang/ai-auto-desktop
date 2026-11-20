import {test} from 'node:test';
import assert from 'node:assert/strict';
import {spawn,execFile} from 'node:child_process';
import {once} from 'node:events';
import {writeFile} from 'node:fs/promises';
import {createInterface} from 'node:readline';
import {join,resolve} from 'node:path';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

test('100 browser and native desktop steps copy the right receipt each time',{
  skip:!process.env.AAD_TEST_CHROME || !process.env.AAD_ATSPI_FIXTURE_BINARY,timeout:120000
}, async context=>{
  const page=await fixture((_request,response)=>{
    response.setHeader('content-type','text/html');
    response.end('<title>Hybrid receipts</title><input aria-label="Receipt" readonly><button onclick="field.value=\'ORD-\'+(++count)">Generate</button><script>window.count=0;const field=document.querySelector("input")</script>');
  });
  context.after(()=>page.close());
  const native=spawn(process.env.AAD_ATSPI_FIXTURE_BINARY,[],{env:{...process.env,AAD_TEST_REPORT_EDITS:'1'},stdio:['ignore','pipe','inherit']});
  context.after(()=>native.kill());
  const lines=createInterface({input:native.stdout}), edits=[];
  lines.on('line',line=>{if(line.startsWith('EDIT:'))edits.push(line.slice(5));});
  assert.deepEqual(await once(lines,'line'),['READY']);
  const browser={provider:'browser.cdp',url:page.url};
  const desktop={provider:'desktop.linux_atspi',title:'AAD Qt AT-SPI Fixture'};
  const phases=[];
  for(let index=1;index<=25;index++){
    const id='receipt_'+index,expected='ORD-'+index;
    phases.push({app:browser,steps:[
      {locator:{name:'Generate'},action:{type:'click'}},
      {id,read:[{as:id,locator:{name:'Receipt'},property:'value'}],expect:{[id]:expected}}]});
    phases.push({app:desktop,steps:[
      {locator:{role:'text',name:'Qt fixture text entry'},action:{type:'fill',value:'${{ steps.'+id+'.output.values.'+id+' }}'}},
      {id:'confirm_'+index,read:[{as:'native_'+index,locator:{role:'text',name:'Qt fixture text entry'},property:'value'}],expect:{['native_'+index]:expected}}]});
  }
  const file=join(page.root,'hybrid.yaml');
  await writeFile(file,JSON.stringify({kind:'Recipe',name:'hybrid',phases}));
  const cli=process.env.AAD_TEST_CLI || resolve('target/debug/aad');
  const start=performance.now();
  const {stdout}=await promisify(execFile)(cli,['run',file,'--permission','desktop.observe','--permission','desktop.input'],{
    env:{...process.env,AAD_CDP_PORT:String(page.port),AAD_CONTROL_LEASE_DIR:join(page.root,'leases')},maxBuffer:2**20});
  const result=JSON.parse(stdout);
  assert.equal(result.executed_steps,100);
  assert.equal(result.outputs.native_25,'ORD-25');
  assert.equal(await page.cdp.eval('window.count'),25);
  assert.deepEqual(edits,Array.from({length:25},(_,index)=>'ORD-'+(index+1)));
  context.diagnostic(`100 mixed steps; 25 receipts verified by the Qt app; ${Math.round(performance.now()-start)} ms`);
});
