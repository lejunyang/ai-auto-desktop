import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {resolve} from 'node:path';
import {test} from 'node:test';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

test('read a visible canvas ledger using real cell OCR through the CLI', {
  skip: process.platform!=='linux' || !process.env.AAD_TEST_CHROME, timeout: 90000
}, async t => {
  const page=await fixture((req,res)=>{
    if(req.url==='/pixel.svg'){
      res.setHeader('Content-Type','image/svg+xml');
      return res.end('<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4"/></svg>');
    }
    res.setHeader('Content-Type','text/html');
    res.end(`<!doctype html><canvas aria-label="Ledger" width=600 height=180></canvas>
    <script>
    const el=document.querySelector('canvas'),ctx=el.getContext('2d');
    ctx.fillStyle='white';ctx.fillRect(0,0,600,180);
    ctx.fillStyle='#333';ctx.font='bold 28px sans-serif';
    for(const [x,y,label] of [[16,39,'NORTH'],[216,39,'125'],[416,39,'OPEN'],
      [16,99,'SOUTH'],[216,99,'840'],[416,99,'PAID'],[16,159,'WEST'],[216,159,'317']])
      ctx.fillText(label,x,y);
    ctx.fillStyle='#888';
    for(const x of [0,200,400])ctx.fillRect(x,0,1,180);
    for(const y of [0,60,120])ctx.fillRect(0,y,600,1);
    </script>`);
  });
  t.after(()=>page.close());
  const spec={locator:{name:'Ledger'},rows:3,columns:['region','amount','state']};
  const cli=process.env.AAD_TEST_CLI || resolve('target/debug/aad');
  const read=async (s=spec,env=process.env)=>JSON.parse((await promisify(execFile)(cli,[
    'browser','read','--browser-port',String(page.port),'--url',page.url,'--canvas-grid',JSON.stringify(s)
  ],{env})).stdout);
  const result=await read();
  assert.deepEqual(result.cells.map(c=>c.text),['NORTH','125','OPEN','SOUTH','840','PAID','WEST','317',null]);
  for(const [i,c] of result.cells.entries()){
    assert.equal(c.row,Math.floor(i/3)+1);
    assert.equal(c.column,spec.columns[i%3]);
    assert.deepEqual(c.bounds,{left:i%3*200,top:Math.floor(i/3)*60,width:200,height:60});
    if(c.text!==null)assert.ok(c.confidence>=0&&c.confidence<=1);
    assert.equal(c.uncertain,c.confidence===null||c.confidence<result.confidence_threshold||c.truncated);
  }
  assert.equal(result.cells.at(-1).uncertain,true);
  assert.equal(result.cells.at(-1).confidence,null);
  assert.equal(result.declared_cells,9);
  assert.equal(result.returned_cells,9);
  assert.equal(result.truncated,false);
  assert.equal(result.dataset_complete,null);
  t.diagnostic(JSON.stringify({texts:result.cells.map(c=>c.text),uncertain_cells:result.uncertain_cells}));

  // With a broken engine configuration, a scope error proves rejection
  // happened before even constructing the OCR provider.
  const noOcr={...process.env,OCR_TESSERACT_COMMAND:'invalid JSON'};
  const refuse=async (code,s=spec)=>assert.rejects(read(s,noOcr),e=>{
    const body=JSON.parse(e.stdout);
    assert.equal(body.code,code,JSON.stringify(body));return true;
  });
  await refuse('CONTROL.INVALID_REQUEST',{...spec,rows:0});
  await refuse('CONTROL.INVALID_REQUEST',{...spec,columns:['amount','amount']});
  await page.cdp.eval("el.insertAdjacentHTML('afterend','<button aria-label=Other>Other</button>');true");
  await refuse('CONTROL.ACTION_UNSUPPORTED',{...spec,locator:{name:'Other'}});
  await page.cdp.eval("el.insertAdjacentHTML('afterend','<div aria-label=Ledger></div>');true");
  await refuse('CONTROL.AMBIGUOUS');
  await page.cdp.eval("document.querySelector('div').remove();el.style.marginTop='200vh';true");
  await refuse('CONTROL.CANVAS_SCOPE_UNPROVEN');
  assert.equal(await page.cdp.eval('scrollY'),0);
  await page.cdp.eval("el.style.marginTop='0';el.insertAdjacentHTML('afterend','<div style=\"position:fixed;left:30px;top:31px;width:12px;height:12px;background:red;pointer-events:none\"></div>');true");
  await refuse('CONTROL.CANVAS_SCOPE_UNPROVEN');
  await page.cdp.eval("document.querySelector('div').remove();el.insertAdjacentHTML('afterend','<input type=password style=\"position:fixed;left:30px;top:31px\">');true");
  await refuse('CONTROL.ACTION_UNSUPPORTED');
  await page.cdp.eval("document.querySelector('input').remove();ctx.clearRect(1,1,10,10);true");
  await refuse('CONTROL.CANVAS_SCOPE_UNPROVEN');
  await page.cdp.eval("el.height=300;ctx.fillStyle='white';ctx.fillRect(0,0,el.width,el.height);true");
  const capped=await read({...spec,rows:23});
  assert.equal(capped.returned_cells,64);
  assert.equal(capped.declared_cells,69);
  assert.equal(capped.truncated,true);
  assert.deepEqual(capped.truncation_reasons,['cell_limit']);
  assert.equal(capped.dataset_complete,null);
  await page.cdp.eval(`(async()=>{const img=new Image();img.src='http://localhost:${new URL(page.url).port}/pixel.svg';
    await img.decode();ctx.drawImage(img,0,0);return true;})()`);
  await refuse('CONTROL.CANVAS_SCOPE_UNPROVEN');
});
