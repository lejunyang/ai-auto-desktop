// Interactive Windows desktop, Node 22+, PowerShell 7, an English/Chinese file
// dialog. Build aad, set AAD_TEST_CHROME to the Chrome/Edge executable, then run
// node --test tests/windows/native-browser-picker.test.mjs
import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {mkdtemp, readFile, rm, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join, resolve} from 'node:path';
import {test} from 'node:test';
import {setTimeout as delay} from 'node:timers/promises';
import {promisify} from 'node:util';
import {fixture} from '../browser/fixture.mjs';

const run = promisify(execFile);
const html = `<!doctype html><meta charset=utf-8><title>Native picker fixture</title>
<input type=file id=upload><button id=save>Save a receipt</button>
<script>
  upload.onchange = async event => {
    if (event.isTrusted) window.uploaded = await upload.files[0].text();
  };
  save.onclick = async () => {
    try {
      const handle = await showSaveFilePicker({suggestedName:'receipt.txt',
        types:[{description:'Plain text',accept:{'text/plain':['.txt']}}]});
      const writer = await handle.createWritable();
      await writer.write('receipt from the web page\\r\\n');
      await writer.close();
      window.saved = handle.name;
    } catch (e) { window.saveError = String(e); }
  };
</script>`;

// Observe the HWND independently. A hidden helper console leaves the dialog in front.
async function foregroundDialog() {
  const script = `
Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class PickerFixture {
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr w, StringBuilder s, int n);
}
'@
$until = [DateTime]::UtcNow.AddSeconds(10)
do {
  $w = [PickerFixture]::GetForegroundWindow()
  $s = [Text.StringBuilder]::new(100)
  $null = [PickerFixture]::GetClassName($w, $s, 100)
  if ($s.ToString() -eq '#32770') { Write-Output $w.ToInt64(); exit 0 }
  Start-Sleep -Milliseconds 50
} while ([DateTime]::UtcNow -lt $until)
throw 'No active Common Item Dialog'
`;
  const {stdout} = await run('pwsh', ['-NoProfile', '-NonInteractive', '-Command', script],
    {windowsHide:true, timeout:16000});
  return stdout.trim();
}

async function waitFor(read, expected) {
  for (let i = 0; i < 120; i++) {
    if (await read() === expected) return;
    await delay(50);
  }
  assert.equal(await read(), expected);
}

test('bind the Windows Open and Save dialogs and verify the selected bytes', {
  skip:process.platform !== 'win32' || !process.env.AAD_TEST_CHROME, timeout:150000
}, async t => {
  const root = await mkdtemp(join(tmpdir(), 'aad picker '));
  t.after(() => rm(root, {recursive:true, force:true, maxRetries:10, retryDelay:100}));
  const page = await fixture((_, res) => res.end(html), ['--lang=en-US'], {headed:true});
  t.after(() => page.close());
  const cli = process.env.AAD_TEST_CLI || resolve('target/debug/aad.exe');
  function select(kind, id, path) {
    const grant = kind === 'choose-files' ? '--allow-upload-dir' : '--allow-save-dir';
    return run(cli, ['browser', kind, '--cdp-port', String(page.port), '--page', page.target.id,
      '--allow-input', '--window', id, grant, root, '--file', path], {windowsHide:true, timeout:35000});
  }
  async function click(id) {
    const result = await page.cdp.call('Runtime.evaluate',
      {expression:'document.getElementById('+JSON.stringify(id)+').click()', userGesture:true});
    assert.equal(result.exceptionDetails, undefined);
    return foregroundDialog();
  }
  const source = join(root, '源 file.txt');
  const destination = join(root, 'new 收据.txt');
  await writeFile(source, 'independent upload bytes');
  const open = await click('upload');
  await assert.rejects(select('choose-files', '1', source));
  await assert.rejects(select('save-as', open, destination));
  assert.equal(await foregroundDialog(), open);
  assert.equal(await page.cdp.eval('upload.files.length'), 0);
  const uploaded = JSON.parse((await select('choose-files', open, source)).stdout);
  assert.equal(uploaded.picker_closed, true);
  await waitFor(() => page.cdp.eval('window.uploaded'), 'independent upload bytes');

  const save = await click('save');
  await assert.rejects(select('save-as', save, source));
  await assert.rejects(select('choose-files', save, source));
  assert.equal(await foregroundDialog(), save);
  assert.equal(await readFile(source, 'utf8'), 'independent upload bytes');
  const saved = JSON.parse((await select('save-as', save, destination)).stdout);
  assert.equal(saved.picker_closed, true);
  assert.equal(saved.destination_selected, true);
  await waitFor(() => page.cdp.eval('window.saved || window.saveError'), 'new 收据.txt');
  assert.equal(await readFile(destination, 'utf8'), 'receipt from the web page\r\n');
});
