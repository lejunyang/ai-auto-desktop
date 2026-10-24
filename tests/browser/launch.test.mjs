import {test} from 'node:test';
import assert from 'node:assert/strict';
import {createServer} from 'node:http';
import {spawn} from 'node:child_process';
import {mkdtemp,readFile,rm,stat,lstat,writeFile,mkdir} from 'node:fs/promises';
import {join,resolve,dirname} from 'node:path';
import {tmpdir} from 'node:os';
import {setTimeout as delay} from 'node:timers/promises';
import {fixture} from './fixture.mjs';

const aad=process.env.AAD_TEST_CLI || resolve('target/debug/aad');
function command(args,executable=aad) {
  const child=spawn(executable,args,{stdio:['ignore','pipe','pipe']});let stdout='',stderr='';
  child.stdout.on('data',d=>stdout+=d);child.stderr.on('data',d=>stderr+=d);
  const done=new Promise((resolve,reject)=>{child.on('error',reject);child.on('exit',(code,signal)=>resolve({code,signal,stdout,stderr}));});
  return {child,done};
}
async function until(fn) {
  for(let i=0;i<200;i++){try{const result=await fn();if(result)return result;}catch{}await delay(50);}
  throw new Error('Timed out waiting for launched recorder');
}
const json=async path=>JSON.parse(await readFile(path,'utf8'));
const exists=async path=>stat(path).then(()=>true,()=>false);

test('launch records initial traffic, reuses profiles and cleans only owned browser state', {skip:!process.env.AAD_TEST_CHROME,timeout:90000},async()=>{
  const root=await mkdtemp(join(tmpdir(),'aad-launch-test-'));
  const requests=[];
  const server=createServer((req,res)=>{
    requests.push({url:req.url,cookie:req.headers.cookie});
    res.setHeader('Content-Type',req.url.startsWith('/initial')?'application/json':'text/html');
    if(req.url.startsWith('/initial'))res.end('{"token":"initial-response-secret"}');
    else res.end(`<!doctype html><title>Launch fixture</title><h1>Launch recording</h1><script>
      fetch('/initial?token=initial-query-secret');
      fetch('/state?previous='+encodeURIComponent(localStorage.getItem('fixture')));
      localStorage.setItem('fixture','persisted');
      document.cookie='fixture=persisted-cookie; Max-Age=3600; Path=/';
    </script>`);
  });
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  const url=`http://127.0.0.1:${server.address().port}/`;
  const base=['browser-record','start','--browser','edge','--browser-executable',process.env.AAD_TEST_CHROME,'--headless','--url',url,'--seconds','15'];
  let running;
  async function record(name,extra=[],check) {
    const out=join(root,name);const requestStart=requests.length;
    running=command([...base,'--out',out,...extra]);
    try {
      const manifest=await until(async()=>{const m=await json(join(out,'manifest.json'));return m.status==='recording'&&m;});
      await until(()=>requests.slice(requestStart).some(r=>r.url.startsWith('/state?')));
      if(check)await check(manifest,out);
      await delay(600);
      const stop=await command(['browser-record','stop','--out',out]).done;
      assert.equal(stop.code,0,stop.stderr);
      const result=await running.done;running=undefined;
      assert.equal(result.code,0,result.stderr+'\n'+result.stdout);
      assert.equal((await json(join(out,'manifest.json'))).status,'complete');
      const network=await readFile(join(out,'evidence/network.jsonl'),'utf8');
      assert.ok(network.includes('initial-query-secret'),'first navigation XHR must be recorded raw');
      assert.ok(network.includes('initial-response-secret'),'first navigation response must be recorded');
      assert.ok(network.split('\n').filter(Boolean).map(JSON.parse).some(r=>r.request?.url===url),'initial Document request must be recorded');
      assert.ok(await exists(join(out,'walkthrough.md')));
      await assert.rejects(fetch(`http://127.0.0.1:${manifest.options.port}/json/version`),'owned browser must close');
      return {result,requests:requests.slice(requestStart)};
    }catch(e){if(running){running.child.kill('SIGINT');await running.done;running=undefined;}throw e;}
  }
  try {
    const fresh=await record('fresh');
    const temporary=fresh.result.stderr.match(/with profile (.*); it closes/)[1];
    assert.equal(await exists(temporary),false,'fresh temporary profile must be removed');
    assert.ok(fresh.requests.some(r=>r.url==='/state?previous=null'));

    const profile=join(root,'working-profile');
    await record('persistent-first',['--profile-dir',profile],async(manifest)=>{
      await writeFile(join(profile,'keep-user-file'),'keep');
      const refused=await command([...base,'--out',join(root,'busy'),'--profile-dir',profile]).done;
      assert.notEqual(refused.code,0,'concurrent profile use must be refused');
      assert.equal(await exists(join(root,'busy')),false);
      assert.equal((await (await fetch(`http://127.0.0.1:${manifest.options.port}/json/list`)).json()).filter(t=>t.type==='page').length,1,'failed launch cannot open another page or close original browser');
      const invalid=await command([...base,'--out',join(root,'mixed'),'--page','existing-page']).done;
      assert.notEqual(invalid.code,0,'attach and launch are mutually exclusive');
    });
    assert.equal(await readFile(join(profile,'keep-user-file'),'utf8'),'keep');
    assert.equal(await exists(join(profile,'.aad-recording.lock')),false);
    const reused=await record('persistent-second',['--profile-dir',profile]);
    assert.ok(reused.requests.some(r=>r.url==='/state?previous=persisted'),'localStorage persists when reusing full profile');
    assert.ok(reused.requests.some(r=>r.url==='/'&&r.cookie?.includes('fixture=persisted-cookie')),'cookie persists across browser restart');

    const listing=await command(['browser-record','profiles','--browser','edge','--profile-source',profile]).done;
    assert.equal(listing.code,0,listing.stderr);
    assert.deepEqual(JSON.parse(listing.stdout).profiles.map(p=>p.id),['Default']);
    const copied=await record('login-copy',['--existing-profile','--profile-source',profile]);
    assert.ok(copied.result.stderr.includes('Using existing profile Default'),'bare flag selects the only profile');
    assert.ok(copied.requests.some(r=>r.url==='/'&&r.cookie?.includes('fixture=persisted-cookie')),'login copy must carry the fixture cookie');
    assert.equal(await exists(copied.result.stderr.match(/with profile (.*); it closes/)[1]),false,'temporary login copy is cleaned');
    assert.equal(await readFile(join(profile,'keep-user-file'),'utf8'),'keep','copy source remains intact');
    await mkdir(join(profile,'Profile 1'));
    await writeFile(join(profile,'Profile 1','Preferences'),'{}');
    const multiple=await command([...base,'--out',join(root,'ambiguous'),'--existing-profile','--profile-source',profile]).done;
    assert.notEqual(multiple.code,0);
    assert.match(multiple.stdout+multiple.stderr,/multiple profiles.*Default.*Profile 1/);
    assert.equal(await exists(join(root,'ambiguous')),false,'ambiguity cannot start recording');
    await record('explicit',['--existing-profile','Default','--profile-source',profile]);
  }finally{
    if(running){running.child.kill('SIGINT');await running.done;}
    await new Promise(resolve=>server.close(resolve));
    await rm(root,{recursive:true,force:true,maxRetries:10,retryDelay:100});
  }
});

test('occupied non-CDP profiles require opt-in termination and unrelated browsers survive', {skip:!process.env.AAD_TEST_CHROME,timeout:90000},async()=>{
  const root=await mkdtemp(join(tmpdir(),'aad-busy-profile-test-'));
  const profile=join(root,'source with spaces');
  const other=await fixture((req,res)=>res.end('<title>Unrelated browser</title>'));
  const base=['browser-record','start','--browser','edge','--browser-executable',process.env.AAD_TEST_CHROME,'--headless','--seconds','1','--no-screenshots'];
  let owner;
  async function occupy() {
    owner=spawn(process.env.AAD_TEST_CHROME,['--headless=new',`--user-data-dir=${profile}`,'--no-first-run','--no-default-browser-check','about:blank'],{stdio:'ignore'});
    await until(async()=>await lstat(join(profile,'SingletonLock')).then(()=>true,()=>false) || process.platform==='win32'&&await exists(join(profile,'lockfile')));
    await delay(500);
    assert.equal(owner.exitCode,null);
  }
  async function assertAlive() {
    assert.equal(owner.exitCode,null,'source browser must remain running');
    assert.equal(owner.signalCode,null);
    assert.equal(await other.cdp.eval('document.title'),'Unrelated browser');
  }
  try {
    const seed=await command([...base,'--out',join(root,'seed'),'--profile-dir',profile]).done;
    assert.equal(seed.code,0,seed.stderr+seed.stdout);
    await occupy();
    const selection=['--existing-profile','--profile-source',profile];
    const refused=await command([...base,'--out',join(root,'blocked'),...selection]).done;
    assert.notEqual(refused.code,0);assert.match(refused.stdout+refused.stderr,/--kill-browser/);
    await assertAlive();assert.equal(await exists(join(root,'blocked')),false);
    const helper=join(dirname(aad),process.platform==='win32'?'aad-browser.exe':'aad-browser');
    const helperRefused=await command(['--browser','edge','--existing-profile','--profile-source',profile,'--port','19333'],helper).done;
    assert.notEqual(helperRefused.code,0);assert.match(helperRefused.stderr,/--kill-browser/);
    await assertAlive();
    const invalid=await command([...base,'--out',join(root,'invalid'),'--existing-profile','Missing','--profile-source',profile,'--kill-browser']).done;
    assert.notEqual(invalid.code,0);await assertAlive();
    const invalidCopy=await command([...base,'--out',join(root,'bad-copy'),...selection,'--profile-dir',profile,'--kill-browser']).done;
    assert.notEqual(invalidCopy.code,0);await assertAlive();
    const restart=await command([...base,'--out',join(root,'restarted'),...selection,'--kill-browser']).done;
    assert.equal(restart.code,0,restart.stdout+restart.stderr);
    await until(()=>owner.exitCode!==null||owner.signalCode!==null);
    assert.equal(await other.cdp.eval('document.title'),'Unrelated browser');
    assert.ok(await exists(join(profile,'Default','Preferences')));
    await occupy();
    const reuse=await command([...base,'--out',join(root,'reused'),'--profile-dir',profile,'--kill-browser']).done;
    assert.equal(reuse.code,0,reuse.stdout+reuse.stderr);
    await until(()=>owner.exitCode!==null||owner.signalCode!==null);
    assert.equal(await other.cdp.eval('document.title'),'Unrelated browser');
  } finally {
    if(owner?.exitCode===null&&owner.signalCode===null) {owner.kill('SIGTERM');await new Promise(resolve=>owner.once('exit',resolve));}
    await other.close();
    await rm(root,{recursive:true,force:true,maxRetries:20,retryDelay:100});
  }
});
