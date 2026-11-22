import assert from 'node:assert/strict';
import {execFile} from 'node:child_process';
import {mkdtemp, readFile, rm, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import test from 'node:test';
import {pathToFileURL} from 'node:url';
import {promisify} from 'node:util';

const browser = process.env.AAD_TEST_HEADLESS_CHROMIUM, fabric = process.env.AAD_TEST_FABRIC_MODULE;
test('interactive Fabric Canvas publishes on the actual hit-tested input layer', {
  skip: browser && fabric ? false : 'requires AAD_TEST_HEADLESS_CHROMIUM and AAD_TEST_FABRIC_MODULE',
}, async () => {
  const directory = await mkdtemp(join(tmpdir(), 'aad-fabric-hit-'));
  try {
    const moduleURL = async path => 'data:text/javascript;base64,' + (await readFile(path)).toString('base64');
    const fixture = join(directory, 'index.html');
    await writeFile(fixture, `<!doctype html><canvas id="lower"></canvas><pre id="result">not run</pre><script type="module">
      import {Canvas, Rect} from '${await moduleURL(fabric)}';
      import {attachFabricScene} from '${await moduleURL(new URL('./fabric.mjs', import.meta.url))}';
      document.querySelector('script').remove();
      try {
        const canvas = new Canvas(document.getElementById('lower'), {width:200, height:100, renderOnAddRemove:false});
        const rect = new Rect({left:20, top:15, width:40, height:25, strokeWidth:0});
        canvas.add(rect);
        const publisher = attachFabricScene(canvas);
        publisher.target.setAttribute('aria-label','Diagram editor');
        canvas.setViewportTransform([2,0,0,2,-20,-10]);
        canvas.renderAll();
        const locator = document.querySelector('canvas[aria-label="Diagram editor"]');
        globalThis.__aadControlV1 = {scope_element: () => locator};
        const graph = ${await readFile(new URL('../../crates/aad-control/src/browser/editor_scene.js', import.meta.url), 'utf8')}({});
        const box = graph.objects[0].box, bounds = locator.getBoundingClientRect();
        const clientX = bounds.left + (box.fx + box.fw/2)*bounds.width;
        const clientY = bounds.top + (box.fy + box.fh/2)*bounds.height;
        const hit = document.elementFromPoint(clientX, clientY);
        let picked = false;
        canvas.on('mouse:down', ({target}) => { picked = target === rect; });
        hit.dispatchEvent(new MouseEvent('mousedown', {bubbles:true, clientX, clientY, button:0, buttons:1}));
        document.dispatchEvent(new MouseEvent('mouseup', {bubbles:true, clientX, clientY, button:0}));
        const evidence = {upperHit:hit === canvas.upperCanvasEl, locatorHit:hit === locator,
          lowerAbsent:!canvas.getElement().hasAttribute('data-aad-scene'), picked, status:graph.objects[0].status};
        publisher.dispose();
        await canvas.dispose();
        document.getElementById('result').textContent = JSON.stringify(evidence);
      } catch (error) { document.getElementById('result').textContent = String(error); }
    </script>`);
    const {stdout} = await promisify(execFile)(browser, ['--headless', '--disable-gpu', '--disable-dev-shm-usage',
      '--disable-background-networking', '--disable-component-update', '--no-first-run', '--no-proxy-server',
      `--user-data-dir=${join(directory, 'profile')}`, '--dump-dom', pathToFileURL(fixture).href],
    {timeout: 30_000, maxBuffer: 2_000_000});
    const text = stdout.match(/<pre id="result">([^<]*)<\/pre>/)?.[1];
    assert.equal(text, JSON.stringify({upperHit:true, locatorHit:true, lowerAbsent:true, picked:true, status:'confirmed'}));
  } finally {
    await rm(directory, {recursive:true, force:true});
  }
});
