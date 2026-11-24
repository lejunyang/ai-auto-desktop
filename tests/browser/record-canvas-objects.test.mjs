import assert from 'node:assert/strict';
import {execFile, spawn} from 'node:child_process';
import {once} from 'node:events';
import {readFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {setTimeout as delay} from 'node:timers/promises';
import {test} from 'node:test';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

const chromium = {skip:!process.env.AAD_TEST_CHROME,timeout:60000};
const exec = promisify(execFile);
async function until(check) {
  for (let tries=0;tries<120;tries++) {
    if (await check().catch(()=>false)) return;
    await delay(50);
  }
  throw Error('recording did not reach checkpoint');
}

async function setup(t, source) {
  const page=await fixture((_req,res)=>{
    res.setHeader('Content-Type','text/html; charset=utf-8');
    res.end(`<!doctype html><canvas aria-label="Approval flow" width=620 height=260></canvas><script>
      const canvas=document.querySelector('canvas'), ctx=canvas.getContext('2d');
      const nodes=[['pay','PAY',20,35],['approved','REVIEW',300,35],['other','REVIEW',300,150]];
      let edge=true, moveOnClick=true; const hits=[];
      function draw(){
        ctx.fillStyle='white';ctx.fillRect(0,0,620,260);ctx.fillStyle=ctx.strokeStyle='#151515';ctx.lineWidth=3;ctx.font='bold 24px sans-serif';
        for(const [,label,x,y] of nodes){ctx.strokeRect(x,y,140,65);ctx.fillText(label,x+15,y+42)}
        const middle=nodes[0][3]+33;
        if(edge){ctx.beginPath();ctx.moveTo(160,middle);ctx.lineTo(298,middle);ctx.stroke();
          ctx.beginPath();ctx.moveTo(298,middle);ctx.lineTo(284,middle-7);ctx.lineTo(284,middle+7);ctx.closePath();ctx.fill()}
        if (${source==='editor'}) canvas.dataset.aadScene=JSON.stringify({objects:nodes.map(([id,label,x,y])=>({id,label,kind:'rectangle',box:{fx:x/620,fy:y/260,fw:140/620,fh:65/260}})),edges:[{from:'pay',to:'approved'}]});
      }
      draw();canvas.onclick=e=>{
        const r=canvas.getBoundingClientRect(),px=(e.clientX-r.left)*620/r.width,py=(e.clientY-r.top)*260/r.height;
        const hit=nodes.find(([,,x,y])=>x<=px && px<x+140 && y<=py && py<y+65);
        hits.push([e.isTrusted,hit?.[0]]);
        if(moveOnClick){moveOnClick=false; for(const node of nodes)node[3]=185-node[3]; draw()}
      };
    </script>`);
  });
  t.after(()=>page.close());
  const cli=process.env.AAD_TEST_CLI || resolve('target/debug/aad'),out=join(page.root,'recording');
  const env={...process.env,AAD_CDP_PORT:String(page.port),AAD_RECORDINGS_DIR:join(page.root,'store'),AAD_CONTROL_LEASE_DIR:join(page.root,'leases')};
  const run=async (...args)=>JSON.parse((await exec(cli,args,{env})).stdout);
  const recorder=spawn(cli,['browser-record','start','--cdp-port',String(page.port),'--page',page.target.id,'--out',out,'--no-screenshots'],{env,stdio:'ignore'});
  const stopped=once(recorder,'exit');
  t.after(()=>{if(recorder.exitCode===null) recorder.kill()});
  const evidence=()=>readFile(join(out,'evidence/actions.jsonl'),'utf8');
  await until(async()=>JSON.parse(await readFile(join(out,'manifest.json'),'utf8')).status==='recording');
  let clickNumber=0;
  const click=async(id)=>{
    const point=await page.cdp.eval(`(()=>{const node=nodes.find(([id])=>id===${JSON.stringify(id)});const rect=canvas.getBoundingClientRect();
      const x=rect.left+(node[2]+24)*rect.width/620,y=rect.top+(node[3]+26)*rect.height/260;if(document.elementFromPoint(x,y)!==canvas)throw Error('fixture point covered');return {x,y}})()`);
    await page.cdp.call('Input.dispatchMouseEvent',{type:'mouseMoved',...point});
    const marker=`pointer ready ${++clickNumber}`;
    await run('browser-record','mark','--out',out,'--note',marker);
    await until(async()=>(await evidence()).includes(marker));
    await page.cdp.eval(`new Promise(resolve=>{const watcher=new IntersectionObserver(entries=>{watcher.disconnect();resolve(entries[0].isVisible)}, {trackVisibility:true,delay:100}); watcher.observe(canvas)})`);
    for(const type of ['mousePressed','mouseReleased'])await page.cdp.call('Input.dispatchMouseEvent',{type,button:'left',clickCount:1,...point});
  };
  const finish=async(count)=>{
    await until(async()=>(await evidence()).split('\n').filter(row=>row.includes('"canvas_click"')).length>=count);
    await run('browser-record','stop','--out',out);
    assert.equal((await stopped)[0],0);
    const exported=await run('browser-record','steps','--out',out,'--save','diagram');
    const rows=(await readFile(join(out,'steps.jsonl'),'utf8')).trim().split('\n').map(JSON.parse).filter(row=>row.action?.kind==='canvas_click');
    return {exported,rows};
  };
  const replay=(name='diagram')=>run('run',name,'--permission','desktop.observe','--permission','desktop.input');
  return {page,click,finish,replay,out,run};
}

for(const source of ['editor','flowchart'])test(`${source}: replay the recorded object after an immediate redraw swaps duplicate labels`,chromium,async t=>{
  const {page,click,finish,replay,out,run}=await setup(t,source);
  await click('approved');
  assert.deepEqual(await page.cdp.eval('hits'),[[true,'approved']]);
  const {exported,rows}=await finish(1);
  const query=source==='editor'?{id:'approved'}:{label:'REVIEW',after:'PAY'};
  assert.deepEqual(rows[0].action.canvas,{source,query},JSON.stringify(rows[0].action));
  assert.deepEqual(exported.skipped,[]);
  assert.deepEqual(exported.steps.map(step=>step.action),[{type:'click_object',query,options:{source}}]);
  assert.deepEqual(exported.steps[0].locator,{role:'canvas',name:'Approval flow'});
  assert.match(await readFile(join(out,'walkthrough.md'),'utf8'),source==='editor'?/approved/:/REVIEW/);
  if(source==='editor')await page.cdp.eval("nodes[1][1]='RENAMED';draw()");
  assert.equal((await replay()).status,'succeeded');
  assert.deepEqual(await page.cdp.eval('hits'),[[true,'approved'],[true,'approved']]);
  if(source==='flowchart'){
    await page.cdp.eval('edge=false;draw()');
    await assert.rejects(replay());
    assert.equal(await page.cdp.eval('hits.length'),2);
  } else {
    const recipe=join(out,'diagram.yaml');
    await run('recipe',exported.saved.recording_path,'--out',recipe);
    assert.equal((await replay(recipe)).status,'succeeded');
    assert.deepEqual(await page.cdp.eval('hits.at(-1)'),[true,'approved']);
  }
});

test('invalid editor graphs, overlapping hits and synthetic clicks never become canvas-centre clicks',chromium,async t=>{
  const {page,click,finish}=await setup(t,'editor');
  await page.cdp.eval(`moveOnClick=false;nodes[1][3]=nodes[2][3];draw()`);
  await click('approved');
  await page.cdp.eval(`canvas.dataset.aadScene='inflate'.repeat(200000)`);
  await click('approved');
  await page.cdp.eval('draw();canvas.click()');
  const {exported,rows}=await finish(3);
  assert.equal(exported.count,0);
  assert.deepEqual(exported.skipped.map(s=>s.reason),Array(3).fill('canvas_object_unresolved'));
  assert.ok(rows.every(row=>row.action.canvas===null && JSON.stringify(row.action).length<2048));
});

test('pixel duplicates without a distinguishing relation stay unresolved after redraw',chromium,async t=>{
  const {page,click,finish}=await setup(t,'flowchart');
  await page.cdp.eval('edge=false;draw();canvas.addEventListener("click",()=>{edge=true;draw()})');
  await click('approved');
  const {exported}=await finish(1);
  assert.equal(exported.count,0);
  assert.equal(exported.skipped[0].reason,'canvas_object_unresolved');
});

for(const transform of ['none','scale(.85)'])test(`DPR and scroll (${transform}): freeze the visible pixel region and replay after the board moves`,chromium,async t=>{
  const {page,click,finish,replay}=await setup(t,'flowchart');
  const dpi=deviceScaleFactor=>page.cdp.call('Emulation.setDeviceMetricsOverride',{mobile:false,width:860,height:640,deviceScaleFactor});
  await dpi(2);
  await page.cdp.eval(`const view=document.createElement('div');view.style='overflow:auto;width:300px;height:150px';
    canvas.before(view);view.append(canvas);canvas.style='display:block;width:775px;height:325px;transform-origin:0 0;transform:${transform}';
    view.scrollLeft=300;view.scrollTop=12;nodes[1][1]='READY';draw();true`);
  await click('approved');
  const {exported,rows}=await finish(1);
  assert.deepEqual(await page.cdp.eval('[devicePixelRatio,view.scrollLeft,view.scrollTop,...hits]'),[2,300,12,[true,'approved']]);
  assert.deepEqual(exported.steps.map(s=>s.action.query),[{label:'READY'}],JSON.stringify(rows));
  await dpi(1.5);
  await page.cdp.eval('view.scrollTop=145;true');
  assert.equal((await replay()).status,'succeeded');
  assert.deepEqual(await page.cdp.eval('[devicePixelRatio,...hits]'),[1.5,[true,'approved'],[true,'approved']]);
});

test('a pointer-transparent overlay elsewhere on the canvas rules out pixel evidence',chromium,async t=>{
  const {page,click,finish}=await setup(t,'flowchart');
  await page.cdp.eval(`const cover=document.createElement('span');cover.style='position:absolute;left:18px;top:20px;width:90px;height:50px;background:white;pointer-events:none';document.body.append(cover);true`);
  await click('approved');
  const {exported,rows}=await finish(1);
  assert.deepEqual(await page.cdp.eval('hits'),[[true,'approved']]);
  assert.equal(rows[0].action.canvas,null);
  assert.deepEqual(exported.skipped.map(step=>step.reason),['canvas_object_unresolved']);
});
