import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {readFile, writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import test from 'node:test';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

const exec=promisify(execFile);
const locator={name:'Board'};
test('find and click full-canvas objects after scrolling and zooming a large board',{
  skip:!process.env.AAD_TEST_CHROME,timeout:90000
}, async t=>{
  const page=await fixture((_req,res)=>{
    res.setHeader('Content-Type','text/html');
    res.end(`<!doctype html><style>
      body{margin:8px}#view{overflow:auto;width:480px;height:240px}canvas{display:block;transform-origin:0 0}
      </style><div id="view" role="region" aria-label="Board view"><canvas aria-label="Board" width="3600" height="1600"></canvas></div>
      <script>
      const cv=document.querySelector('canvas'),view=document.querySelector('#view'),ctx=cv.getContext('2d');
      ctx.fillStyle='white';ctx.fillRect(0,0,3600,1600);
      ctx.strokeStyle=ctx.fillStyle='#181818';ctx.lineWidth=5;ctx.font='bold 38px sans-serif';
      for(const [x,y,label] of [[40,50,'START'],[1700,760,'REVIEW']]){
        ctx.strokeRect(x,y,270,105);ctx.fillText(label,x+24,y+62);
      }
      ctx.fillText('WEST',1724,908);
      function show(scale){cv.style.width=(3600*scale)+'px';cv.style.height=(1600*scale)+'px';
        view.scrollLeft=1700*scale-40;view.scrollTop=760*scale-35;}
      window.hits=[];
      cv.onclick=e=>{const r=cv.getBoundingClientRect(),x=(e.clientX-r.left)*3600/r.width,y=(e.clientY-r.top)*1600/r.height;
        hits.push(e.isTrusted && x>1700 && x<1970 && y>760 && y<865);};
      </script>`);
  });
  t.after(()=>page.close());
  const args=['--browser-port',String(page.port),'--url',page.url];
  const cli=process.env.AAD_TEST_CLI||resolve('target/debug/aad');
  const env={...process.env,AAD_CONTROL_LEASE_DIR:join(page.root,'leases')};
  const run=async (...flags)=>JSON.parse((await exec(cli,flags,{env})).stdout);
  const read=(...extra)=>run('browser','read',...args,'--canvas-objects',JSON.stringify(locator),...extra);
  const script=join(page.root,'click.js');
  await writeFile(script,'return page.clickObject({name:"Board"}, inputs)');
  const click=q=>run('browser','script',script,...args,'--allow-input','--inputs',JSON.stringify(q));
  const initial=await read();
  assert.equal(initial.clipped,true);
  assert.deepEqual(initial.objects.map(o=>o.label),['START']);
  const scrollScript=join(page.root,'scroll.js');
  await writeFile(scrollScript,'return page.scroll({name:"Board view"}, "down", 2)');
  await run('browser','script',scrollScript,...args,'--allow-input');
  for(let i=0;i<30 && !(await page.cdp.eval('view.scrollTop>100'));i++) await new Promise(r=>setTimeout(r,30));
  assert.ok((await read()).visible_region.fy>0.06);
  for(const scale of [0.5,1,1.25]){
    await page.cdp.eval(`show(${scale});true`);
    const file=join(page.root,`view-${scale}.svg`);
    const scene=await read('--preview',file);
    assert.deepEqual(scene.objects.map(o=>o.label),['REVIEW'], JSON.stringify({scale,region:scene.visible_region,layout:await page.cdp.eval('[view.scrollLeft,view.scrollTop,cv.getBoundingClientRect().toJSON()]')}));
    assert.ok(scene.visible_region.fx>0.38 && scene.visible_region.fw<0.4);
    const b=scene.objects[0].box;
    assert.ok(Math.abs(b.fx-1700/3600)<0.007 && Math.abs(b.fy-760/1600)<0.012);
    const svg=await readFile(file,'utf8');
    const previewWidth=Number(svg.match(/<svg[^>]+ width="(\d+)"/)[1]);
    const previewLeft=Number(svg.match(/<rect x="([-\d.]+)"/)[1]);
    assert.ok(Math.abs(previewLeft/previewWidth - 40/465)<0.015,String(previewLeft));
    const words=(await run('browser','read',...args,'--canvas-text',JSON.stringify(locator))).lines.flatMap(l=>l.words);
    assert.ok(words.some(w=>w.text==='WEST' && w.box.fx>0.46 && w.box.fx<0.5),JSON.stringify(words));
    await click({label:'REVIEW'});
    assert.equal(await page.cdp.eval('hits.at(-1)'),true);
  }
  await assert.rejects(click({id:initial.objects[0].id,digest:initial.digest}),e=>
    JSON.parse(e.stdout).error.code==='CONTROL.STALE_TARGET');
  await page.cdp.call('Emulation.setDeviceMetricsOverride',{mobile:false,width:860,height:640,deviceScaleFactor:2});
  await page.cdp.eval('show(0.75);true');
  assert.deepEqual((await read()).objects.map(o=>o.label),['REVIEW']);
  await click({label:'REVIEW'});
  assert.equal(await page.cdp.eval('devicePixelRatio'),2);
  await page.cdp.eval("cv.style.width='3600px';cv.style.height='1600px';cv.style.transform='scale(.75)';true");
  assert.deepEqual((await read()).objects.map(o=>o.label),['REVIEW']);
  await click({label:'REVIEW'});
  assert.deepEqual(await page.cdp.eval('hits'),[true,true,true,true,true]);
  await page.cdp.call('Emulation.setPageScaleFactor',{pageScaleFactor:1.5});
  assert.deepEqual((await read()).objects.map(o=>o.label),['REVIEW']);
  await click({label:'REVIEW'});
  assert.deepEqual(await page.cdp.eval('hits'),Array(6).fill(true));
  await page.cdp.eval(`document.body.insertAdjacentHTML('beforeend','<div style="position:fixed;left:60px;top:60px;width:7px;height:8px;background:red;pointer-events:none"></div>');true`);
  await assert.rejects(read(),e=>JSON.parse(e.stdout).code==='CONTROL.CANVAS_SCOPE_UNPROVEN');
  await page.cdp.eval(`const cover=document.body.lastElementChild;
    cover.style='position:fixed;left:62px;top:62px;width:0;height:0;pointer-events:none';cover.id='cover';
    const style=document.createElement('style');style.textContent='#cover::before{content:"";position:absolute;width:6px;height:6px;background:red}';cover.append(style);true`);
  await assert.rejects(read(),e=>JSON.parse(e.stdout).code==='CONTROL.CANVAS_SCOPE_UNPROVEN');
  await page.cdp.call('Emulation.setPageScaleFactor',{pageScaleFactor:1});
  await page.cdp.eval(`document.body.lastElementChild.remove();cv.style.transform='none';show(.75);view.scrollTo(0,0);true`);
  const view_hint={fx:.515,fy:.51};
  await assert.rejects(click({label:'UNKNOWN',view_hint}),e=>JSON.parse(e.stdout).error.code==='CONTROL.NOT_FOUND');
  assert.equal(await page.cdp.eval('hits.length'),6);
  await page.cdp.eval('view.scrollTo(0,0);true');
  await click({label:'REVIEW',view_hint});
  assert.deepEqual(await page.cdp.eval('hits'),Array(7).fill(true));
  await page.cdp.eval(`
    cv.dataset.aadScene=JSON.stringify({objects:[{id:'review',kind:'rectangle',label:'REVIEW',
      box:{fx:1700/3600,fy:760/1600,fw:270/3600,fh:105/1600}}],edges:[]});view.scrollTo(0,0);true`);
  await click({id:'review'});
  assert.deepEqual(await page.cdp.eval('hits'),Array(8).fill(true));
  assert.ok((await page.cdp.eval('view.scrollLeft'))>800);

});
