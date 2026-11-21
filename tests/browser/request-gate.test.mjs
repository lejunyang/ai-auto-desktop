import assert from 'node:assert/strict';
import {once} from 'node:events';
import {execFile, spawn} from 'node:child_process';
import {createServer} from 'node:http';
import {resolve, join} from 'node:path';
import {readFile,writeFile} from 'node:fs/promises';
import {createInterface} from 'node:readline';
import {setTimeout as delay} from 'node:timers/promises';
import {promisify} from 'node:util';
import {test} from 'node:test';
import {fixture} from './fixture.mjs';

test('request gate covers browser traffic and reports policy decisions', {
  skip: !process.env.AAD_TEST_CHROME, timeout: 195000
}, async t => {
  const received = [], asked = [], policyPaths = [];
  let decision = 'allow', status = 200, revision = 1, stall = false, disconnect = false;
  let watch = [{host:'127.0.0.1', methods:['POST']}];
  let token = 'fixture-bearer';
  const gate = createServer(async (req,res) => {
    policyPaths.push(req.url);
    assert.equal(req.headers.authorization, 'Bearer '+token);
    let reply = {revision:String(revision), watch};
    if (req.method === 'POST') {
      let data = '';
      for await (const bytes of req) data += bytes;
      asked.push(JSON.parse(data));
      if(disconnect){req.socket.destroy();return;}
      if(stall)return;
      reply = {decision, message:'fixture rule '+endpoint+' '+token, askId:'fixture-ask'};
    }
    res.writeHead(status, {'Content-Type':'application/json'});
    res.end(JSON.stringify(reply));
  });
  await new Promise(r=>gate.listen(0,'127.0.0.1',r));
  t.after(()=>new Promise(r=>gate.close(r)));
  const endpoint = 'http://127.0.0.1:'+gate.address().port+'/policy';
  const page = await fixture((req,res)=>{
    received.push([req.method,req.url]);
    res.setHeader("Access-Control-Allow-Origin","*");
    if(req.url==='/worker.js'){
      res.setHeader('Content-Type','application/javascript');
      res.end('fetch("/worker-write",{method:"POST"}).then(()=>postMessage("sent"),()=>postMessage("blocked"))');
      return;
    }
    if(req.url==='/cross-frame'){
      res.setHeader("Content-Type","text/html");
      res.end('<script>fetch('+JSON.stringify(page.url+'child-write')+',{method:"POST"}).then(()=>parent.postMessage("sent","*"),()=>parent.postMessage("blocked","*"))</script>');
      return;
    }
    if(req.url==='/redirect'){
      res.writeHead(307,{Location:page.url+'destination'});res.end();return;
    }
    res.setHeader('Content-Type','text/html');
    res.end('<title>Fixture page</title><button>Save</button><button>Plain</button><script>document.querySelector("button").onclick=()=>fetch("/write?token=sensitive",{method:"POST",headers:{"Content-Type":"application/json"},body:"{}"}).catch(()=>{});</script>');
  });
  t.after(()=>page.close());
  const cli=process.env.AAD_TEST_CLI || resolve('target/debug/aad');
  const env={...process.env,HOME:page.root,LOCALAPPDATA:page.root,AAD_CONTROL_LEASE_DIR:join(page.root,"leases")};
  for(const k of ['OMEM_URL','OMEM_TOKEN','AAD_REQUEST_GATE_URL','AAD_REQUEST_GATE_TOKEN']) delete env[k];
  const file=join(page.root,'click.js');
  await writeFile(file,'page.click({name:"Save"});return true;');
  const args=['browser-script',file,'--url',page.url,'--browser-port',String(page.port),'--allow-input'];
  const samples=async f=>{
    const times=[];
    for(let i=0;i<11;i++){const start=performance.now();await f();times.push(performance.now()-start);}
    return times.sort((a,b)=>a-b)[5].toFixed(1);
  };
  const fetchPath=(method)=>page.cdp.eval('fetch("/timing",{method:'+JSON.stringify(method)+'}).then(r=>r.text()).then(()=>true)');
  const baseline=await samples(()=>fetchPath('GET'));
  const plainFile=join(page.root,'plain.js');
  await writeFile(plainFile,'page.click({name:"Plain"});');
  const sampleClick=async()=>{
    const times=[];
    for(let i=0;i<5;i++){
      const {stdout}=await promisify(execFile)(cli,[args[0],plainFile,...args.slice(2)],{env});
      times.push(JSON.parse(stdout).elapsed_ms);
    }
    return times.sort((a,b)=>a-b)[2];
  };
  const plainClick=await sampleClick();
  await promisify(execFile)(cli,args,{env});
  assert.equal(asked.length,0);
  assert.equal(received.filter(([,u])=>u.startsWith('/write')).length,1);
  Object.assign(env,{AAD_REQUEST_GATE_URL:endpoint,AAD_REQUEST_GATE_TOKEN:token});
  const configuredClick=await sampleClick();
  status=503;
  await assert.rejects(promisify(execFile)(cli,args,{env}),e=>{
    assert.equal(JSON.parse(e.stdout).error.details.blocked_requests[0].decision,'unavailable');
    return true;
  });
  status=200;
  decision='deny';
  await assert.rejects(promisify(execFile)(cli,args,{env}),e=>{
    const report=JSON.parse(e.stdout);
    assert.equal(report.error?.details?.blocked_requests?.[0]?.decision,'deny',e.stdout);
    return true;
  });
  const child=spawn(cli,['mcp','--browser-port',String(page.port),'--allow-input'],{env,stdio:['pipe','pipe','inherit']});
  t.after(()=>{child.stdin.end();child.kill()});
  const lines=createInterface({input:child.stdout})[Symbol.asyncIterator]();
  let id=0;
  async function call(name,arguments_){
    child.stdin.write(JSON.stringify({jsonrpc:'2.0',id:++id,method:'tools/call',params:{name,arguments:arguments_}})+'\n');
    let line;
    do {line=(await lines.next()).value;}while(JSON.parse(line).id!==id);
    assert.ok(JSON.parse(line).result,line);
    for(const secret of [endpoint,token,'?token=sensitive']) assert.ok(!line.includes(secret));
    return JSON.parse(JSON.parse(line).result.content[0].text);
  }
  const script=source=>call('browser_script',{source,url:page.url});
  decision='pending';
  const report=await script('page.click({name:"Save"});return true;');
  const block=report.error.details.blocked_requests[0];
  const listed=await call('control_targets',{});
  const resource=listed.data.resources.find(r=>r.metadata?.page_id===page.target.id);
  const diagnostics=await call('browser_network',{resource_id:resource.resource_id});
  assert.ok(diagnostics.requests.some(r=>r.request_gate?.decision==='pending'));
  assert.equal(block.askId,'fixture-ask');
  assert.equal(block.decision,'pending');
  assert.match(block.hint,/approval/);
  assert.equal(asked.length,2);
  assert.equal(asked[1].method,'POST');
  assert.equal(asked[1].url,page.url+'write?token=sensitive');
  assert.deepEqual(asked[1].page,{url:page.url,title:'Fixture page'});
  assert.equal(asked[1].body,'{}');
  assert.ok(asked[1].session);
  assert.equal(received.filter(([,u])=>u.startsWith('/write')).length,1);
  decision='allow';
  const action=await script('page.click({name:"Save"});return true;');
  assert.equal(action.result,true);
  t.diagnostic('Median local fetch round trip (11 samples): ungated GET '+baseline+' ms, configured GET '+await samples(()=>fetchPath('GET'))+' ms, gated POST '+await samples(()=>fetchPath('POST'))+' ms. Median script click without a fetch (5 samples): ungated '+plainClick+' ms, configured '+configuredClick+' ms.');
  assert.equal(received.filter(([,u])=>u.startsWith('/write')).length,2);
  const run=(path,method='GET')=>page.cdp.eval('fetch('+JSON.stringify(path)+',{method:'+JSON.stringify(method)+'}).then(()=>true,()=>false)');
  const body='a'.repeat(65535)+'é'.repeat(1000);
  await page.cdp.eval('fetch("/text",{method:"POST",headers:{"Content-Type":"text/plain"},body:'+JSON.stringify(body)+'}).then(r=>r.ok)');
  assert.equal(asked.at(-1).bodyTruncated,true);
  assert.equal(Buffer.byteLength(asked.at(-1).body),65535);
  const n=asked.length;
  assert.equal(await run('/read'),true);
  assert.equal(asked.length,n);
  for(const code of [401,503]){
    status=code;
    const blocked=await script('page.click({name:"Save"});');
    assert.match(blocked.error.details.blocked_requests[0].message,code===401?/expired/:/unavailable/);
    assert.equal(await run('/failure','POST'),false);
    assert.equal(await run('/read'),true);
  }
  assert.ok(!received.some(([,u])=>u==='/failure'));
  status=200;
  token='replacement-bearer';
  const beforeRotation=asked.length;
  await promisify(execFile)(cli,['apps'],{env:{...env,AAD_CDP_PORT:String(page.port),AAD_REQUEST_GATE_TOKEN:token}});
  assert.equal(asked.length,beforeRotation);
  assert.equal((await script('page.click({name:"Save"});return true;')).result,true);
  assert.equal(asked.length,beforeRotation+1);
  decision='deny';
  const other=page.url.replace('127.0.0.1','localhost');
  await run(other+'redirect','POST');
  assert.ok(received.some(([,u])=>u==='/redirect'));
  assert.ok(!received.some(([,u])=>u==='/destination'));
  assert.ok(asked.some(r=>r.url===page.url+'destination'));
  await page.cdp.eval('new Promise(r=>{const f=document.createElement("iframe");f.src="/frame";f.onload=()=>r(true);document.body.append(f)})');
  assert.equal(await page.cdp.eval('frames[0].fetch("/frame-write",{method:"POST"}).then(()=>true,()=>false)'),false);
  assert.equal(await page.cdp.call('Runtime.evaluate',{userGesture:true,awaitPromise:true,returnByValue:true,expression:'new Promise(r=>{const w=open("/popup");w.onload=()=>w.fetch("/popup-write",{method:"POST"}).then(()=>r(true),()=>r(false))})'}).then(v=>v.result.value),false);
  const xo = await page.cdp.eval('new Promise(r=>{window.addEventListener("message",e=>r(e.data),{once:true});const f=document.createElement("iframe");f.src='+JSON.stringify(other+'cross-frame')+';document.body.append(f)})');
  assert.equal(xo,'blocked');
  assert.ok(asked.some(r=>r.url===page.url+'child-write'));
  assert.ok(!received.some(([,u])=>['/frame-write','/popup-write'].includes(u)));
  const worker=await page.cdp.eval('new Promise(r=>{window.testWorker=new Worker("/worker.js");testWorker.onmessage=e=>r(e.data)})');
  if(worker==='sent'){
    const {targetInfos}=await page.cdp.call('Target.getTargets');
    const {targetId}=targetInfos.find(t=>t.type==='worker' && t.url===page.url+'worker.js');
    const {sessionId}=await page.cdp.call('Target.attachToTarget',{targetId,flatten:true});
    await assert.rejects(page.cdp.call('Fetch.enable',{},sessionId),/32601/);
    await page.cdp.call('Target.detachFromTarget',{sessionId});
  }else{
    assert.equal(worker,'blocked');
    assert.ok(!received.some(([,u])=>u==='/worker-write'));
  }
  await page.cdp.eval('testWorker.terminate()');
  const out=join(page.root,'recording');
  const recorderEnv={...env};
  delete recorderEnv.AAD_REQUEST_GATE_URL;
  delete recorderEnv.AAD_REQUEST_GATE_TOKEN;
  const recording=promisify(execFile)(cli,['browser-record','start','--cdp-port',String(page.port),'--page',page.target.id,'--out',out,'--seconds','30','--no-screenshots','--redact-network'],{env:recorderEnv});
  for(let i=0;i<100;i++){
    try{if(JSON.parse(await readFile(join(out,'manifest.json'),'utf8')).status==='recording')break;}catch{}
    await delay(50);
  }
  assert.equal(await run('/recorded','POST'),false);
  await delay(300);
  await promisify(execFile)(cli,['browser-record','stop','--out',out],{env});
  await recording;
  const evidence=await readFile(join(out,'evidence','network.jsonl'),'utf8');
  assert.ok(!evidence.includes(endpoint) && !evidence.includes(token));
  assert.ok(evidence.trim().split('\n').map(JSON.parse).some(r=>r.request_gate?.decision==='deny'));
  disconnect=true;
  const lost=await script('page.click({name:"Save"});');
  assert.match(lost.error.details.blocked_requests[0].message,/unavailable/);
  disconnect=false;
  stall=true;
  const waiting=script('page.click({name:"Save"});');
  await delay(100);
  assert.equal(await run('/other-during-approval'),true);
  stall=false;
  assert.equal(await run('/parallel','POST'),false);
  assert.ok(asked.at(-1).url.endsWith('/parallel'));
  const expired=await waiting;
  assert.match(expired.error.details.blocked_requests[0].message,/timed out/);
  stall=false;
  watch=[{host:'127.0.0.1',methods:['DELETE']}];revision++;
  for(let i=0;i<60 && !(await run('/changed','POST'));i++)await delay(250);
  assert.equal(await run('/changed','DELETE'),false);
  watch=[];revision++;
  for(let i=0;i<60 && !(await run('/refresh','DELETE'));i++)await delay(250);
  const after=asked.length;
  assert.equal(await run('/refresh','DELETE'),true);
  assert.equal(asked.length,after);
  // A new caller using the host's environment hands off a new policy. Clients
  // without credentials can still see the active owner's decisions.
  const hostEnv={...env,OMEM_URL:endpoint.replace('/policy',''),OMEM_TOKEN:token,AAD_CDP_PORT:String(page.port)};
  delete hostEnv.AAD_REQUEST_GATE_URL;
  delete hostEnv.AAD_REQUEST_GATE_TOKEN;
  watch=[{host:null,methods:['POST']}];revision++;
  await promisify(execFile)(cli,['apps'],{env:hostEnv});
  assert.equal(policyPaths.at(-1),'/api/operation-gate/http');
  assert.equal(await run('/host-env','POST'),false);
  assert.equal(policyPaths.at(-1),'/api/operation-gate/http');
  stall=true;
  const closing=run('/closing','POST');
  for(let i=0;i<100 && !asked.some(r=>r.url.endsWith('/closing'));i++)await delay(10);
  assert.ok(asked.some(r=>r.url.endsWith('/closing')));
  const exit=once(child,'exit');
  child.stdin.end();
  await exit;
  assert.equal(await closing,false);
  assert.ok(!received.some(([,u])=>u==='/closing'));
  const count=asked.length;
  assert.equal(await run('/after-close','POST'),true);
  assert.equal(asked.length,count);

});

test('cached scope survives a refused connection', {
  skip: !process.env.AAD_TEST_CHROME, timeout: 30000
}, async t => {
  const arrivals = [];
  const page = await fixture((req,res) => {
    arrivals.push(req.url);
    res.setHeader('Content-Type','text/html');
    res.end('<button onclick="fetch(\'/ordinary\');fetch(\'/protected\',{method:\'POST\'}).catch(()=>{})">Submit</button>');
  });
  t.after(()=>page.close());
  const gate = createServer((req,res) => {
    assert.equal(req.method,'GET');
    res.setHeader('Connection','close');
    res.end(JSON.stringify({revision:'1',watch:[{host:'127.0.0.1',methods:['POST']}]}));
    gate.close();
  });
  await new Promise(resolve=>gate.listen(0,'127.0.0.1',resolve));
  t.after(()=>gate.close());
  const env = {
    ...process.env, HOME:page.root, LOCALAPPDATA:page.root,
    AAD_CONTROL_LEASE_DIR:join(page.root,'leases'),
    AAD_REQUEST_GATE_URL:'http://127.0.0.1:'+gate.address().port+'/policy',
    AAD_REQUEST_GATE_TOKEN:'fixture-bearer'
  };
  const script = join(page.root,'submit.js');
  await writeFile(script,'page.click({role:"button",name:"Submit"});');
  const args = ['browser-script',script,'--url',page.url,'--browser-port',String(page.port),'--allow-input'];
  await assert.rejects(promisify(execFile)(process.env.AAD_TEST_CLI || resolve('target/debug/aad'),args,{env}),err=>{
    const details = JSON.parse(err.stdout).error.details;
    assert.ok(details.blocked_requests,err.stdout);
    const [record] = details.blocked_requests;
    assert.equal(record.decision,'unavailable');
    assert.equal(record.url,page.url+'protected');
    return true;
  });
  assert.ok(arrivals.includes('/ordinary'));
  assert.ok(!arrivals.includes('/protected'));
});

test('an unconfigured MCP session retains policy from a short-lived CLI', {
  skip: !process.env.AAD_TEST_CHROME, timeout: 30000
}, async t => {
  let writes = 0, decisions = 0;
  const page = await fixture((req, res) => {
    if (req.method === 'POST') writes++;
    res.setHeader('Content-Type', 'text/html');
    res.end('<button onclick="fetch(\'/save\',{method:\'POST\'}).catch(()=>{})">Save</button>');
  });
  t.after(() => page.close());
  const gate = createServer((req, res) => {
    assert.equal(req.headers.authorization, 'Bearer fixture-token');
    const scope = {revision:'1', watch:[{host:'127.0.0.1', methods:['POST']}]};
    if (req.method === 'POST') decisions++;
    res.end(JSON.stringify(req.method === 'GET' ? scope : {decision:'deny', message:'Saving is restricted'}));
  });
  await new Promise(resolve => gate.listen(0, '127.0.0.1', resolve));
  t.after(() => gate.close());
  const cli = process.env.AAD_TEST_CLI || resolve('target/debug/aad');
  const env = {...process.env, HOME:page.root, LOCALAPPDATA:page.root,
    AAD_CDP_PORT:String(page.port), AAD_CONTROL_LEASE_DIR:join(page.root, 'leases')};
  for (const key of ['OMEM_URL','OMEM_TOKEN','AAD_REQUEST_GATE_URL','AAD_REQUEST_GATE_TOKEN']) delete env[key];
  const mcp = spawn(cli, ['mcp','--browser-port',String(page.port),'--allow-input'], {env});
  t.after(() => mcp.kill());
  const lines = createInterface({input:mcp.stdout})[Symbol.asyncIterator]();
  let id = 0;
  async function save() {
    const params = {name:'browser_script', arguments:{url:page.url, source:'page.click({role:"button",name:"Save"}); return true;'}};
    mcp.stdin.write(JSON.stringify({id:++id, jsonrpc:'2.0', method:'tools/call', params})+'\n');
    const response = JSON.parse((await lines.next()).value);
    assert.equal(response.id, id);
    return JSON.parse(response.result.content[0].text);
  }
  const initial = await save();
  assert.equal(initial.result, true, JSON.stringify(initial));
  assert.equal(writes, 1);

  await promisify(execFile)(cli, ['apps'], {env:{...env,
    AAD_REQUEST_GATE_URL:'http://127.0.0.1:'+gate.address().port+'/policy',
    AAD_REQUEST_GATE_TOKEN:'fixture-token'}});
  // The idle MCP has had no opportunity to discover the owner in another action.
  await delay(100);
  assert.equal(await page.cdp.eval('fetch("/save",{method:"POST"}).then(()=>true,()=>false)'), false);
  const {error} = await save();
  assert.equal(error.code, 'BROWSER.REQUEST_BLOCKED');
  const [record] = error.details.blocked_requests;
  assert.equal(record.decision, 'deny');
  assert.equal(record.message, 'Saving is restricted');
  assert.match(record.hint, /Do not try another route/);
  assert.equal(decisions, 2);
  assert.equal(writes, 1);
});

async function scriptActors(t, page, decide) {
  const policy = createServer(async (req, res) => {
    let reply = {revision:'1', watch:[{host:'127.0.0.1', methods:['POST']}]};
    if (req.method === 'POST') {
      let data = '';
      for await (const chunk of req) data += chunk;
      reply = await decide(JSON.parse(data));
    }
    res.end(JSON.stringify(reply));
  });
  await new Promise(resolve => policy.listen(0, '127.0.0.1', resolve));
  t.after(() => policy.close());
  const bin = process.env.AAD_TEST_CLI || resolve('target/debug/aad');
  const env = {...process.env, HOME:page.root, LOCALAPPDATA:page.root,
    AAD_CONTROL_LEASE_DIR:join(page.root,'leases'), AAD_REQUEST_GATE_TOKEN:'fixture-token',
    AAD_REQUEST_GATE_URL:'http://127.0.0.1:'+policy.address().port+'/policy'};
  const flags = ['--browser-port',String(page.port),'--allow-input'];
  const child = spawn(bin, ['mcp',...flags], {env});
  t.after(() => child.kill());
  const lines = createInterface({input:child.stdout})[Symbol.asyncIterator]();
  let id = 0;
  async function tool(name, args) {
    child.stdin.write(JSON.stringify({jsonrpc:'2.0',id:++id,method:'tools/call',params:{name,arguments:args}})+'\n');
    const reply = JSON.parse((await lines.next()).value);
    assert.equal(reply.id, id);
    return reply.result;
  }
  return {
    async cli(source) {
      const file = join(page.root, 'action.js');
      await writeFile(file, source);
      try {
        const {stdout} = await promisify(execFile)(bin, ['browser-script',file,'--url',page.url,...flags], {env});
        return JSON.parse(stdout);
      } catch (error) {
        if (!error.stdout) throw error;
        const report = JSON.parse(error.stdout);
        assert.ok(report.error, error.stdout);
        return report;
      }
    },
    async mcp(source) {
      const reply = await tool('browser_script', {source,url:page.url});
      return JSON.parse(reply.content[0].text);
    },
    tool
  };
}

test('actions report policy decisions on later redirect hops', {
  skip: !process.env.AAD_TEST_CHROME, timeout:30000
}, async t => {
  let status = 307;
  const arrivals = [], asked = [];
  const page = await fixture((req,res) => {
    if (req.method === 'POST') arrivals.push(req.url);
    if (req.url === '/redirect') {
      res.writeHead(status, {Location:'/destination'}).end();
    } else {
      res.setHeader('Content-Type','text/html');
      res.end('<title>Fixture</title><button onclick="fetch(\'/redirect\',{method:\'POST\'}).catch(()=>{})">Save</button>');
    }
  });
  t.after(() => page.close());
  let decision, firstHop, release;
  const actors = await scriptActors(t, page, async request => {
    assert.equal(request.method, 'POST');
    asked.push(request.url);
    if (request.url === page.url+'redirect') {
      firstHop();
      await new Promise(resolve => { release = resolve; });
      return {decision:'allow'};
    }
    return {decision, message:'Redirect policy', askId:'redirect-approval'};
  });
  for (const [actor, code, verdict] of [[actors.cli,307,'pending'],[actors.cli,308,'allow'],[actors.mcp,308,'deny']]) {
    status = code;
    decision = verdict;
    arrivals.length = asked.length = 0;
    const paused = new Promise(resolve => { firstHop = resolve; });
    const action = actor('page.click({role:"button",name:"Save"}); return true;');
    await Promise.race([paused, action.then(report => assert.fail(JSON.stringify(report)))]);
    const stillWaiting = await Promise.race([action.then(()=>false), delay(250,true)]);
    release();
    assert.equal(stillWaiting, true);
    const report = await action;
    assert.deepEqual(asked, ['redirect','destination'].map(path=>page.url+path));
    if (verdict === 'allow') {
      assert.equal(report.result, true);
      assert.deepEqual(arrivals, ['/redirect','/destination']);
    } else {
      assert.deepEqual(arrivals, ['/redirect']);
      assert.equal(report.error?.code, 'BROWSER.REQUEST_BLOCKED', JSON.stringify(report));
      const [block] = report.error.details.blocked_requests;
      assert.equal(block.url, page.url+'destination');
      assert.equal(block.decision, verdict);
      assert.equal(block.message, 'Redirect policy');
      if (verdict === 'pending') assert.equal(block.askId, 'redirect-approval');
      assert.match(block.hint, verdict === 'pending' ? /approval/ : /Do not try another route/);
    }
  }
});

test('a form opening a new page reports its decision to the submitting action', {
  skip: !process.env.AAD_TEST_CHROME, timeout:30000
}, async t => {
  const arrivals = [], queries = [];
  const ready = new Set();
  let listenersReady;
  const listening = new Promise(resolve => { listenersReady = resolve; });
  const page = await fixture(async (req, res) => {
    if (req.method === 'POST') {
      let body = '';
      for await (const chunk of req) body += chunk;
      arrivals.push([req.url,body]);
    }
    res.setHeader('Content-Type','text/html');
    if (['/old','/other'].includes(req.url)) {
      const id = JSON.stringify(req.url.slice(1));
      res.end('<script>const id='+id+'; const channel=new BroadcastChannel("submit"); channel.onmessage=()=>fetch("/probe-"+id,{method:"POST"}).catch(()=>{}); fetch("/ready-"+id)</script>');
      return;
    }
    if (req.url.startsWith('/ready-')) {
      ready.add(req.url);
      if (ready.size === 2) listenersReady();
    }
    res.end('<title>Form</title><form method="POST" action="/submit" target="_blank"><input type="hidden" name="case" value="form"><button>Submit</button></form><script>const channel=new BroadcastChannel("submit");document.forms[0].onsubmit=()=>channel.postMessage("probe")</script>');
  }, ['--disable-popup-blocking']);
  t.after(() => page.close());
  let decision = 'pending';
  const actors = await scriptActors(t, page, async request => {
    queries.push(request.url);
    return {decision:request.url.endsWith('/submit') ? decision : 'deny',message:'Form policy',askId:'form-approval'};
  });
  const click = 'page.click({role:"button",name:"Submit"}); return true;';
  await page.cdp.call('Target.setDiscoverTargets',{discover:true});
  await page.cdp.eval('open("/old");true');
  await page.cdp.call('Target.createTarget',{url:page.url+'other',background:true});
  await listening;
  const cli = await actors.cli(click);
  assert.equal(cli.error?.code, 'BROWSER.REQUEST_BLOCKED', JSON.stringify({cli,queries,arrivals}));
  assert.equal(cli.error.details.blocked_requests.length, 1);
  const [pending] = cli.error.details.blocked_requests;
  const childTarget = page.cdp.events.find(e => e.method === 'Target.targetCreated' && e.params.targetInfo.targetId === pending.page_id);
  assert.equal(childTarget?.params.targetInfo.openerId, page.target.id);
  assert.equal(pending.decision, 'pending');
  assert.equal(pending.askId, 'form-approval');
  assert.equal(pending.message, 'Form policy');
  assert.match(pending.hint, /approval/);
  assert.deepEqual(arrivals, []);

  async function data(name, args) {
    const result = await actors.tool(name, args);
    assert.equal(result.isError, false, JSON.stringify(result));
    return JSON.parse(result.content[0].text).data;
  }
  async function submit() {
    const {resources} = await data('control_targets', {});
    const {resource_id} = resources.find(r => r.metadata?.page_id === page.target.id);
    const {observation_id} = await data('control_observe',{resource_id});
    const {target} = await data('control_find',{observation_id,locator:{role:'button',name:'Submit'}});
    return actors.tool('control_act',{target,action:{type:'click'}});
  }
  decision = 'deny';
  const denied = await submit();
  assert.equal(denied.isError,true);
  const {error} = JSON.parse(denied.content[0].text);
  assert.equal(error.code,'BROWSER.REQUEST_BLOCKED');
  assert.equal(error.details.blocked_requests.length, 1);
  const [block] = error.details.blocked_requests;
  assert.equal(block.method,'POST');
  assert.equal(block.url,page.url+'submit');
  assert.equal(block.decision,'deny');
  assert.equal(block.message,'Form policy');
  assert.match(block.hint,/Do not try another route/);
  assert.deepEqual(arrivals, []);
  decision = 'allow';
  assert.equal((await submit()).isError, false);
  assert.deepEqual(arrivals,[['/submit','case=form']]);
  assert.deepEqual(queries.sort(), Array(3).fill(['submit','probe-old','probe-other']).flat().map(path=>page.url+path).sort());
});
