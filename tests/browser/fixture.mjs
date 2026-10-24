import {createServer} from 'node:http';
import {spawn} from 'node:child_process';
import {mkdtemp,readFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {setTimeout as delay} from 'node:timers/promises';

export async function connect(url) {
  const ws = new WebSocket(url), pending = new Map(), events = [];
  let sequence = 0;
  ws.onmessage = ({data}) => {
    const message = JSON.parse(data);
    if (message.id) { const task=pending.get(message.id); pending.delete(message.id); task?.(message); }
    else events.push(message);
  };
  await new Promise((resolve,reject)=>{ws.onopen=resolve;ws.onerror=reject;});
  return {
    events, close:()=>ws.close(),
    async call(method,params={}) {
      const id=++sequence;
      return new Promise((resolve,reject)=>{
        const timer=setTimeout(()=>{pending.delete(id);reject(new Error(`Timed out: ${method}`));},10000);
        pending.set(id,message=>{clearTimeout(timer);message.error?reject(new Error(JSON.stringify(message.error))):resolve(message.result);});
        ws.send(JSON.stringify({id,method,params}));
      });
    },
    async eval(expression) {
      const result=await this.call('Runtime.evaluate',{expression,returnByValue:true,awaitPromise:true});
      if(result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails));
      return result.result.value;
    }
  };
}

export async function fixture(handler) {
  const root=await mkdtemp(join(tmpdir(),'aad-observation-test-'));
  const server=createServer(handler);
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  const url=`http://127.0.0.1:${server.address().port}/`;
  const browser=spawn(process.env.AAD_TEST_CHROME, ['--headless=new','--remote-debugging-address=127.0.0.1',
    '--remote-debugging-port=0',`--user-data-dir=${root}`,'--no-first-run','--no-default-browser-check',url],
    {stdio:'ignore',detached:process.platform!=='win32'});
  let cdp;
  const close=async()=>{
    cdp?.close();
    if(process.platform!=='win32') {try{process.kill(-browser.pid,'SIGTERM');}catch{}}
    else browser.kill();
    await delay(100);
    if(process.platform!=='win32') {try{process.kill(-browser.pid,'SIGKILL');}catch{}}
    await new Promise(resolve=>server.close(resolve));
    await rm(root,{recursive:true,force:true,maxRetries:10,retryDelay:100});
  };
  try {
    for(let tries=0;tries<200;tries++) {
      try {
        const port=Number((await readFile(join(root,'DevToolsActivePort'),'utf8')).split('\n')[0]);
        const targets=await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
        const target=targets.find(t=>t.type==='page' && t.url===url);
        if(target) {
          cdp=await connect(target.webSocketDebuggerUrl);
          for(let ready=0;ready<100;ready++) {
            if(await cdp.eval(`document.readyState==='complete' && location.href===${JSON.stringify(url)}`)) return {root,url,port,target,cdp,close};
            await delay(20);
          }
          throw new Error('Fixture document did not finish loading');
        }
      } catch {}
      if(browser.exitCode!==null) throw new Error(`Browser exited ${browser.exitCode}`);
      await delay(50);
    }
    throw new Error('Browser startup timed out');
  } catch(error) {await close();throw error;}
}
