import {test} from 'node:test';
import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {createHash} from 'node:crypto';
import {createServer} from 'node:http';
import {mkdtemp,readFile,readdir,rm,writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {basename,dirname,join,resolve} from 'node:path';
import {promisify} from 'node:util';

const exec = promisify(execFile);
test('download-export waits for the right ready row, verifies the new file and rejects mismatches', {
  skip:!process.env.AAD_TEST_CHROME, timeout:90000
},async t=>{
  const files={old:'date,total\n2026-11-18,9\n',current:'id,date,total,tax\nA1,2026-11-19,7,0.10\nA2,2026-11-19,-2,0.20\n'};
  const downloads=[];
  let navigations=0;
  const server=createServer(async(req,res)=>{
    if(req.url.startsWith('/file/')){
      const task=req.url.slice(6);
      downloads.push(task);
      res.writeHead(200,{'Content-Type':'text/csv','Content-Disposition':'attachment; filename="bill.csv"'});
      return res.end(files[task]);
    }
    if(req.url!=='/'){res.writeHead(404).end();return;}
    if(++navigations===1) await new Promise(resolve=>setTimeout(resolve,9000));
    res.setHeader('Content-Type','text/html; charset=utf-8');
    res.end(`<title>Export jobs</title><table>
      <tr aria-label="JOB-01"><td>2026-11-18</td><td><a href="/file/old">Get bill</a></td></tr>
      <tr aria-label="JOB-02"><td aria-label="2026-11-19">2026-11-19</td><td id="status">Queued</td></tr></table>
      <script>document.querySelector('a').click();
      const refresh=setInterval(()=>{const row=document.querySelector('#status').parentNode;row.replaceWith(row.cloneNode(true))},20);
      setTimeout(()=>{clearInterval(refresh);document.querySelector('#status').innerHTML='<a href="/file/current">Get bill</a>'},700)</script>`);
  });
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  t.after(()=>new Promise(resolve=>server.close(resolve)));
  const temp=await mkdtemp(join(tmpdir(),'e-'));
  t.after(()=>rm(temp,{recursive:true,force:true}));
  const url=`http://127.0.0.1:${server.address().port}/`;
  const spec={task_id:'JOB-02',required_names:['2026-11-19'],trigger:{role:'link',name:'Get bill'},
    expected:{filename:'bill.csv',bytes:Buffer.byteLength(files.current),sha256:createHash('sha256').update(files.current).digest('hex')}};
  async function invoke(data,timeout=10000,extra=[]){
    const path=join(temp,'spec.json');
    await writeFile(path,JSON.stringify(data));
    const family=/edge/.test(basename(process.env.AAD_TEST_CHROME))?'edge':'chrome';
    const args=['browser','download-export','--headless','--url',url,'--spec',path,'--allow-input','--browser',family,
      '--browser-executable',process.env.AAD_TEST_CHROME,'--timeout-ms',String(timeout),...extra];
    try { return {ok:true,report:JSON.parse((await exec(process.env.AAD_TEST_CLI||resolve('target/debug/aad'),args,{env:{...process.env,TMPDIR:temp,TEMP:temp,TMP:temp}})).stdout)}; }
    catch(err){assert.ok(err.stdout,err.stderr||err.message);return {ok:false,report:JSON.parse(err.stdout)};}
  }
  const collection=join(temp,'expected.json');
  await writeFile(collection,JSON.stringify({complete:true,row_count:2,row_key:'id',columns:['id','date','total','tax'],records:[
    {id:'A2',date:'2026-11-19',total:'-2.00',tax:'0.2'},{id:'A1',date:'2026-11-19',total:'7.00',tax:'0.1'}
  ]}));
  const compare=['--compare-collection',collection,'--decimal','total','--decimal','tax'];
  const run=await invoke(spec,900000,compare);
  assert.equal(run.ok,true,JSON.stringify(run));
  assert.equal(run.report.completed_actions,1);
  const {file,verified,task_id,matched_names}=run.report.result;
  assert.equal(verified,true);
  assert.equal(task_id,'JOB-02');
  assert.deepEqual(matched_names,['2026-11-19']);
  assert.equal(run.report.result.comparison.matches,true);
  assert.deepEqual(run.report.result.comparison.totals,{total:{expected:'5',actual:'5'},tax:{expected:'0.3',actual:'0.3'}});
  assert.ok(file.guid && file.frame_id && file.expected_sha256_checked && file.expected_bytes_checked);
  assert.equal(file.sha256,spec.expected.sha256);
  assert.equal(file.filename,'bill.csv');
  assert.match(basename(dirname(dirname(file.path))),/^aad-downloads-/);
  assert.match(basename(dirname(file.path)),/^verified-/);
  assert.ok(!(await readdir(temp)).some(entry=>entry.startsWith('aad-record-profile-')));
  assert.equal(await readFile(file.path,'utf8'),files.current);
  for(const changed of [{...spec.expected,sha256:'0'.repeat(64)},{...spec.expected,filename:'wrong.csv'}]){
    const failed=await invoke({...spec,expected:changed});
    assert.equal(failed.ok,false,JSON.stringify(failed));
    assert.equal(failed.report.error?.code,'BROWSER.EXPORT_MISMATCH',JSON.stringify(failed.report));
    assert.equal(failed.report.completed_actions,1);
  }
  const wrongDate=await invoke({...spec,required_names:['2026-11-18']},1500);
  assert.equal(wrongDate.ok,false);
  assert.equal(wrongDate.report.completed_actions,0,JSON.stringify(wrongDate.report));
  const compareCurrent=async()=>await invoke({...spec,expected:{filename:'bill.csv'}},10000,compare);
  files.current='id,date,total,tax\nA1,2026-11-18,8.50,0.10\nX9,2026-11-19,1,0\n';
  const mismatch=await compareCurrent();
  assert.equal(mismatch.ok,false);
  assert.equal(mismatch.report.error?.code,'BROWSER.EXPORT_CONTENT_MISMATCH',JSON.stringify(mismatch.report));
  assert.equal(mismatch.report.completed_actions,1);
  assert.equal(mismatch.report.result.verified,true);
  assert.equal(await readFile(mismatch.report.result.file.path,'utf8'),files.current);
  const diff=mismatch.report.result.comparison;
  assert.equal(diff.matches,false);
  assert.equal(diff.row_key,'id');
  assert.equal(diff.difference_count,4);
  assert.deepEqual(diff.totals,{total:{expected:'5',actual:'9.5'},tax:{expected:'0.3',actual:'0.1'}});
  assert.deepEqual(diff.differences,[
    {kind:'changed',key:'A1',column:'date',expected:'2026-11-19',actual:'2026-11-18'},
    {kind:'changed',key:'A1',column:'total',expected:'7.00',actual:'8.50'},
    {kind:'missing',key:'A2',expected:{id:'A2',date:'2026-11-19',total:'-2.00',tax:'0.2'}},
    {kind:'unexpected',key:'X9',actual:{id:'X9',date:'2026-11-19',total:'1',tax:'0'}}
  ]);
  files.current='id,total\nA1,7\n';
  const malformed=await compareCurrent();
  assert.equal(malformed.ok,false);
  assert.equal(malformed.report.error?.code,'BROWSER.EXPORT_COMPARISON_FAILED',JSON.stringify(malformed.report));
  assert.match(malformed.report.error.message,/header/);
  assert.equal(malformed.report.completed_actions,1);
  assert.equal(await readFile(malformed.report.result.file.path,'utf8'),files.current);
  assert.deepEqual(downloads,['old','current','old','current','old','current','old','old','current','old','current']);
  assert.equal(navigations,6);
});
