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
  const files={old:'date,total\n2026-11-18,9\n',current:'date,total\n2026-11-19,7\n'};
  const downloads=[];
  const server=createServer((req,res)=>{
    if(req.url.startsWith('/file/')){
      const task=req.url.slice(6);
      downloads.push(task);
      res.writeHead(200,{'Content-Type':'text/csv','Content-Disposition':'attachment; filename="bill.csv"'});
      return res.end(files[task]);
    }
    res.setHeader('Content-Type','text/html; charset=utf-8');
    res.end(`<title>Export jobs</title><table>
      <tr aria-label="JOB-01"><td>2026-11-18</td><td><a href="/file/old">Get bill</a></td></tr>
      <tr aria-label="JOB-02"><td aria-label="2026-11-19">2026-11-19</td><td id="status">Queued</td></tr></table>
      <script>document.querySelector('a').click();setTimeout(()=>{document.querySelector('#status').innerHTML='<a href="/file/current">Get bill</a>'},700)</script>`);
  });
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  t.after(()=>new Promise(resolve=>server.close(resolve)));
  const temp=await mkdtemp(join(tmpdir(),'e-'));
  t.after(()=>rm(temp,{recursive:true,force:true}));
  const url=`http://127.0.0.1:${server.address().port}/`;
  const spec={task_id:'JOB-02',required_names:['2026-11-19'],trigger:{role:'link',name:'Get bill'},
    expected:{filename:'bill.csv',bytes:Buffer.byteLength(files.current),sha256:createHash('sha256').update(files.current).digest('hex')}};
  async function invoke(data,timeout=10000){
    const path=join(temp,'spec.json');
    await writeFile(path,JSON.stringify(data));
    const family=/edge/.test(basename(process.env.AAD_TEST_CHROME))?'edge':'chrome';
    const args=['browser','download-export','--headless','--url',url,'--spec',path,'--allow-input','--browser',family,
      '--browser-executable',process.env.AAD_TEST_CHROME,'--timeout-ms',String(timeout)];
    try { return {ok:true,report:JSON.parse((await exec(process.env.AAD_TEST_CLI||resolve('target/debug/aad'),args,{env:{...process.env,TMPDIR:temp,TEMP:temp,TMP:temp}})).stdout)}; }
    catch(err){assert.ok(err.stdout,err.stderr||err.message);return {ok:false,report:JSON.parse(err.stdout)};}
  }
  const run=await invoke(spec,900000);
  assert.equal(run.ok,true,JSON.stringify(run));
  assert.equal(run.report.completed_actions,1);
  const {file,verified,task_id,matched_names}=run.report.result;
  assert.equal(verified,true);
  assert.equal(task_id,'JOB-02');
  assert.deepEqual(matched_names,['2026-11-19']);
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
    assert.equal(failed.report.error.code,'BROWSER.EXPORT_MISMATCH');
    assert.equal(failed.report.completed_actions,1);
  }
  const wrongDate=await invoke({...spec,required_names:['2026-11-18']},1500);
  assert.equal(wrongDate.ok,false);
  assert.equal(wrongDate.report.completed_actions,0);
  assert.deepEqual(downloads,['old','current','old','current','old','current','old']);
});
