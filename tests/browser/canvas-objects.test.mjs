import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {readFile, writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {test} from 'node:test';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

const chromium = {skip: !process.env.AAD_TEST_CHROME, timeout: 90000};
const locator = {name:'Diagram'};
const exec = promisify(execFile);

async function diagram(t) {
  const page = await fixture((_req, res) => {
    res.setHeader('Content-Type', 'text/html');
    res.end(`<!doctype html><meta charset="utf-8"><canvas aria-label="Diagram" width=620 height=260></canvas>
    <script>
    const canvas = document.querySelector('canvas'), ctx = canvas.getContext('2d');
    const nodes = [['p','PAY',20,35], ['a','REVIEW',300,35], ['b','REVIEW',300,150]];
    function draw() {
      ctx.fillStyle='white'; ctx.fillRect(0,0,620,260);
      ctx.strokeStyle=ctx.fillStyle='#151515';ctx.lineWidth=3;ctx.font='bold 24px sans-serif';
      for(const [id,label,x,y] of nodes) {
        ctx.strokeRect(x,y,140,65);ctx.fillText(label,x+15,y+42);
      }
      ctx.beginPath();ctx.moveTo(160,68);ctx.lineTo(298,68);ctx.stroke();
      ctx.beginPath();ctx.moveTo(298,68);ctx.lineTo(284,61);ctx.lineTo(284,75);ctx.closePath();ctx.fill();
    }
    function publish(){
      canvas.dataset.aadScene=JSON.stringify({objects:nodes.map(([id,label,x,y])=>({
        id,label,kind:'rectangle',box:{fx:x/620,fy:y/260,fw:140/620,fh:65/260}
      })),edges:[{from:'p',to:'a'}]});
    }
    draw();window.hits=[];
    canvas.addEventListener('click',e=>{
      const hit=nodes.find(([,label,x,y])=>e.offsetX>=x && e.offsetX<x+140 && e.offsetY>=y && e.offsetY<y+65);
      hits.push([e.isTrusted,hit?.[0]]);
    });
    </script>`);
  });
  t.after(()=>page.close());
  const env = {...process.env, AAD_CONTROL_LEASE_DIR:join(page.root,'leases')};
  const cli = process.env.AAD_TEST_CLI || resolve('target/debug/aad');
  const args = ['--url',page.url,'--browser-port',String(page.port)];
  const run = async (...flags)=>JSON.parse((await exec(cli,flags,{env,maxBuffer:2*1024*1024})).stdout);
  const read = (...opts)=>run('browser','read',...args,'--canvas-objects',JSON.stringify(locator),...opts);
  const script = async (source,inputs,allow)=>{
    const file=join(page.root,'objects.js');
    await writeFile(file,source);
    return run('browser','script',file,...args,...(allow?['--allow-input']:[]),
      '--inputs',JSON.stringify(inputs));
  };
  const click = (query,options={},allow=true)=>script(
    'return page.clickObject(inputs.locator, inputs.query, inputs.options)',{locator,query,options},allow);
  return {page,env,read,click,script};
}

function failure(code, inspect=()=>{}) {
  return e=>{
    const reply=JSON.parse(e.stdout);
    const error=reply.error ?? reply;
    assert.equal(error.code,code,JSON.stringify(reply));
    inspect(error);return true;
  };
}

test('editor relationships resolve duplicate labels after a redraw and guard against hover changes',chromium,async t=>{
  const {page,read,click,script}=await diagram(t);
  await page.cdp.eval('publish();true');
  const preview=join(page.root,'candidates.svg');
  const scene=await read('--preview',preview);
  assert.equal(scene.source,'editor');
  assert.deepEqual(scene.objects.map(n=>n.number),[1,2,3]);
  const svg=await readFile(preview,'utf8');
  assert.match(svg,/data:image\/png;base64,/);
  assert.equal((svg.match(/<rect /g)??[]).length,3);
  await assert.rejects(read('--preview',preview),failure('BROWSER.PREVIEW_FAILED'));
  assert.equal(await readFile(preview,'utf8'),svg);
  assert.equal(scene.edges[0].status,'confirmed');
  const viaScript=await script('return page.readObjects(inputs.locator, {source:"editor"})',{locator},false);
  assert.equal(viaScript.result.digest,scene.digest);
  await assert.rejects(click({label:'REVIEW'}),failure('CONTROL.AMBIGUOUS',e=>{
    assert.deepEqual(e.details.candidates.map(o=>o.id),['a','b']);
  }));
  await assert.rejects(click({id:'a'}, {}, false),failure('POLICY.DENIED'));
  await page.cdp.eval('nodes[1][3]=150;nodes[2][3]=35;draw();publish();true');
  await assert.rejects(click({id:'a',digest:scene.digest}),failure('CONTROL.STALE_TARGET'));
  await click({label:'REVIEW',after:'PAY'});
  assert.deepEqual(await page.cdp.eval('hits'),[[true,'a']]);
  await page.cdp.eval("canvas.onmousemove=()=>{nodes[0][3]=145;draw();publish()};true");
  await assert.rejects(click({id:'p'}),failure('CONTROL.STALE_TARGET'));
  assert.deepEqual(await page.cdp.eval('hits'),[[true,'a']]);
});

test('pure painted flowchart yields labels and a directed relationship used by trusted input',chromium,async t=>{
  const {page,read,click}=await diagram(t);
  const scene=await read('--language','en-US');
  assert.equal(scene.source,'flowchart');
  assert.deepEqual(scene.objects.map(n=>n.label).sort(),['PAY','REVIEW','REVIEW']);
  const pay=scene.objects.find(n=>n.label==='PAY');
  assert.ok(scene.edges.some(e=>e.from===pay.id && e.status==='confirmed'),JSON.stringify(scene.edges));
  await click({label:'REVIEW',after:'PAY'},{language:'en-US'});
  assert.deepEqual(await page.cdp.eval('hits'),[[true,'a']]);
  await page.cdp.eval("canvas.onmousemove=()=>{ctx.fillStyle='#c00';ctx.fillRect(3,3,18,12)};true");
  await assert.rejects(click({label:'PAY'}),failure('CONTROL.STALE_TARGET'));
  assert.deepEqual(await page.cdp.eval('hits'),[[true,'a']]);
});

test('vision candidates can be inspected, pinned, and selected in a real browser',chromium,async t=>{
  const {page,env,read,click}=await diagram(t);
  env.AAD_CANVAS_VISION_PROVIDER=join(page.root,'provider.py');
  await writeFile(env.AAD_CANVAS_VISION_PROVIDER,`import sys,json,base64
r=json.load(sys.stdin)
assert r["description"]=="review symbol" and r["width"]==620 and base64.b64decode(r["data"])[:4]==b"\\x89PNG"
print(json.dumps({"objects":[{"id":i,"kind":"symbol","label":"review symbol","confidence":0.88,"box":{"fx":300/620,"fy":y/260,"fw":140/620,"fh":65/260}} for i,y in [("upper",35),("lower",150)]],"edges":[]}))
`);
  const options={description:'review symbol'};
  const scene=await read('--description',options.description);
  assert.equal(scene.source,'vision');
  assert.equal(scene.objects.length,2);
  await assert.rejects(click({}, options),failure('CONTROL.AMBIGUOUS'));
  await assert.rejects(click({min_confidence:0.95}, options),failure('CONTROL.NOT_FOUND'));
  await click({region:'top_right',min_confidence:0.8}, options);
  await click({id:scene.objects[1].id,digest:scene.digest}, options);
  assert.deepEqual(await page.cdp.eval('hits'),[[true,'a'],[true,'b']]);
});

test('local OWL model grounds and clicks a sketched bicycle in Chromium',{
  skip:!process.env.AAD_TEST_CHROME || !process.env.AAD_CANVAS_VISION_MODEL, timeout:120000
},async t=>{
  const {page,env,read,click}=await diagram(t);
  delete env.AAD_CANVAS_VISION_PROVIDER;
  await page.cdp.eval(`(()=>{
    canvas.width=720;canvas.height=240;ctx.fillStyle='white';ctx.fillRect(0,0,720,240);
    const line=(points,width,color='#25252a')=>{
      ctx.strokeStyle=color;ctx.lineWidth=width;ctx.beginPath();
      points.forEach(([x,y],i)=>i?ctx.lineTo(x,y):ctx.moveTo(x,y));ctx.stroke();
    };
    for(const x of [532,662]){
      ctx.strokeStyle='#25252a';ctx.lineWidth=4;ctx.beginPath();
      ctx.arc(x,156,32,0,2*Math.PI);ctx.stroke();
      for(let i=0;i<8;i++)line([[x,156],[x+Math.cos(i*Math.PI/4)*30,156+Math.sin(i*Math.PI/4)*30]],1,'#757575');
    }
    line([[532,156],[570,99],[595,155],[532,156],[640,99],[595,155]],4);
    line([[662,156],[634,81],[646,81],[649,87]],4);
    line([[574,108],[563,86],[552,85],[574,85]],4);
    line([[595,155],[605,168],[613,168]],2);
    canvas.onclick=e=>window.bikeHit=e.isTrusted && e.offsetX>495 && e.offsetX<702 && e.offsetY>70 && e.offsetY<195;
    return true;
  })()`);
  const description='a drawing of a bicycle';
  const scene=await read('--description',description);
  assert.equal(scene.source,'vision');
  const candidate=scene.objects.find(o=>o.box.fx>0.58 && o.box.fw>0.17 && o.confidence>0.25);
  assert.ok(candidate,JSON.stringify(scene.objects));
  await click({id:candidate.id,digest:scene.digest},{description});
  assert.equal(await page.cdp.eval('window.bikeHit'),true);
});
