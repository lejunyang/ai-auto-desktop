import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {readFile, writeFile} from 'node:fs/promises';
import {join, resolve} from 'node:path';
import {test} from 'node:test';
import {promisify} from 'node:util';
import {fixture} from './fixture.mjs';

test('one CLI invocation traverses Chromium, exports CSV and compares exact amounts',
  {skip: !process.env.AAD_TEST_CHROME, timeout: 60000}, async t => {
    const page = await fixture((_req, res) => {
      res.setHeader('Content-Type','text/html;charset=utf-8');
      res.end(`<table aria-label="Bills"><tr><th>ID</th><th>Amount</th><th>Note</th></tr>
        <tr id="row"><td>B1</td><td>0.10</td><td>A, &quot;quote&quot;</td></tr></table><button>Next</button>
        <script>document.querySelector('button').onclick=e=>{
          document.querySelector('#row').innerHTML='<td>B2</td><td>0.20</td><td>Tea</td>';
          e.target.disabled=true;
        };</script>`);
    });
    t.after(() => page.close());
    const spec={table:{role:'table',name:'Bills'},columns:['ID','Amount','Note'],row_key:'ID',
      expected_rows:2,traversal:{mode:'pagination',next:{role:'button',name:'Next'}}};
    const cli=process.env.AAD_TEST_CLI || resolve('target/debug/aad');
    const args=['browser','collect-table','--url',page.url,'--spec','spec.json','--browser-port',String(page.port)];
    const invoke=(...options)=>promisify(execFile)(cli,[...args,...options],{
      cwd:page.root,env:{...process.env,AAD_CONTROL_LEASE_DIR:join(page.root,'leases')}
    });
    const expected='Note,Amount,ID\r\nTea,0.2,B2\r\n"A, ""quote""",0.100,B1\r\n';
    await writeFile(join(page.root,'expected.csv'),expected);
    await writeFile(join(page.root,'spec.json'),JSON.stringify(spec));
    await assert.rejects(invoke(), error=>error.code===1 && /allow-input/.test(error.stdout));
    assert.equal(await page.cdp.eval('row.cells[0].innerText'),'B1');
    const compare=['--allow-input','--compare-csv','expected.csv','--decimal','Amount'];
    const {stdout}=await invoke(...compare,'--output','actual.csv');
    const report=JSON.parse(stdout);
    assert.equal(report.complete,true);
    assert.equal(report.matches,true);
    assert.deepEqual(report.comparison.totals.Amount,{expected:'0.3',actual:'0.3'});
    assert.deepEqual(report.records.map(r=>r.ID),['B1','B2']);
    assert.equal(await readFile(join(page.root,'actual.csv'),'utf8'),
      'ID,Amount,Note\r\nB1,0.10,"A, ""quote"""\r\nB2,0.20,Tea\r\n');
    assert.equal(await page.cdp.eval('row.cells[0].innerText'),'B2');
    await assert.rejects(invoke(...compare,'--output','partial.csv'),error=>{
      const result=JSON.parse(error.stdout);
      return error.code===1 && result.complete===false && result.stop_reason==='row_count_mismatch';
    });
    await assert.rejects(readFile(join(page.root,'partial.csv')), {code:'ENOENT'});
    delete spec.expected_rows;
    await writeFile(join(page.root,'spec.json'),JSON.stringify(spec));
    await page.cdp.eval(`document.querySelector('table').setAttribute('aria-rowcount','2');
      const next=document.querySelector('button');
      const first=document.createElement('button');first.textContent='First';next.after(first);
      window.firstClicks=0;
      first.onclick=()=>{firstClicks++;setTimeout(()=>{
        row.innerHTML='<td>B1</td><td>0.10</td><td>A, &quot;quote&quot;</td>';
        first.disabled=true;next.disabled=false;
      },180)};
      next.addEventListener('click',()=>{first.disabled=false});
      true`);
    await assert.rejects(invoke(...compare,'--output','suffix.csv'),error=>{
      const result=JSON.parse(error.stdout);
      return error.code===1 && result.stop_reason==='start_unverified' && !result.complete &&
        result.scope==='pagination_from_current_page' && result.records.length===1 && !result.comparison;
    });
    await assert.rejects(readFile(join(page.root,'suffix.csv')), {code:'ENOENT'});
    spec.traversal.first={role:'button',name:'First'};
    await writeFile(join(page.root,'spec.json'),JSON.stringify(spec));
    const rewound=JSON.parse((await invoke(...compare,'--output','full.csv')).stdout);
    assert.equal(rewound.complete,true);
    assert.equal(rewound.scope,'pagination_from_first_page');
    assert.equal(rewound.matches,true);
    assert.equal(rewound.actions,2);
    assert.equal(await page.cdp.eval('firstClicks'),1);
    assert.deepEqual(await readFile(join(page.root,'full.csv')),await readFile(join(page.root,'actual.csv')));
    delete spec.traversal.first;
    spec.expected_rows=2;
    await writeFile(join(page.root,'spec.json'),JSON.stringify(spec));
    await page.cdp.eval("location.reload(); true").catch(()=>{});
    for (let i=0;i<100 && await page.cdp.eval("document.querySelector('#row td')?.innerText").catch(()=>null)!=='B1';i++)
      await new Promise(resolve=>setTimeout(resolve,30));
    await writeFile(join(page.root,'expected.csv'),expected.replace('Tea,0.2','Tea,0.21'));
    await assert.rejects(invoke(...compare),error=>{
      const result=JSON.parse(error.stdout);
      return error.code===1 && result.matches===false && result.comparison.differences[0].key==='B2';
    });
  });
