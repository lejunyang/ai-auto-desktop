import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import test from 'node:test';
import {pathToFileURL} from 'node:url';
import {Script} from 'node:vm';
import {attachFabricScene} from './fabric.mjs';

const recognizer = new Script(readFileSync(new URL('../../crates/aad-control/src/browser/editor_scene.js', import.meta.url), 'utf8') + '({})');
class CanvasElement {
  attrs = new Map();
  hasAttribute(key) { return this.attrs.has(key); }
  getAttribute(key) { return this.attrs.get(key) ?? null; }
  setAttribute(key, value) { this.attrs.set(key, value); }
  removeAttribute(key) { this.attrs.delete(key); }
}
function recognize(target) {
  return structuredClone(recognizer.runInNewContext({HTMLCanvasElement: CanvasElement, TextEncoder,
    __aadControlV1: {scope_element: () => target}}));
}
function checkBox(box, [fx, fy, fw, fh]) {
  for (const [key, expected] of Object.entries({fx, fy, fw, fh})) {
    assert.ok(Math.abs(expected - box[key]) < 1e-12, `${key}: expected ${expected}, got ${box[key]}`);
  }
}

test('clips transformed corners, ignores offscreen/hidden objects and never follows links', () => {
  const element = new CanvasElement(), lower = new CanvasElement(), ctx = {}, handlers = new Map();
  const shape = coords => ({visible: true, type: 'rect', getCoords: () => coords.map(([x, y]) => ({x, y}))});
  const first = shape([[0, 0], [20, 0], [20, 10], [0, 10]]), second = shape([[30, 0], [40, 0], [40, 10], [30, 10]]);
  const outside = shape([[-30, 8], [8, -30], [12, -26], [-26, 12]]);
  const items = [first, second, outside, {...first, visible: false}, {...first, opacity: 0}];
  const canvas = {upperCanvasEl: element, getElement: () => lower, size: () => items.length, getObjects: () => items,
    getWidth: () => 100, getHeight: () => 100, getContext: () => ctx, viewportTransform: [1, 0, 0, 1, 0, 0],
    on(event, handler) { handlers.set(event, handler); return () => handlers.delete(event); }};
  const publisher = attachFabricScene(canvas, {getEdges: () => [{from: first, to: second}, {from: outside, to: first}]});
  assert.equal(publisher.target, element);
  assert.equal(recognize(lower), null);
  assert.throws(() => attachFabricScene(canvas), /already/);
  let graph = recognize(element);
  assert.equal(graph.objects.length, 2);
  const [firstId, secondId] = graph.objects.map(object => object.id);
  assert.deepEqual(graph.edges, [{from: firstId, to: secondId, status: 'confirmed'}]);
  canvas.viewportTransform = [2, 0.5, 1, 2, -20, 0];
  handlers.get('after:render')({ctx: {}});
  checkBox(recognize(element).objects[0].box, [0, 0, 0.2, 0.1]);
  handlers.get('after:render')({ctx});
  graph = recognize(element);
  assert.equal(graph.objects[0].id, firstId);
  checkBox(graph.objects[0].box, [0, 0.05, 0.3, 0.25]);
  items.splice(1, 1);
  assert.equal(publisher.publish(), true);
  assert.deepEqual(recognize(element).edges, []);
  canvas.size = () => 257;
  assert.equal(publisher.publish(), false);
  assert.deepEqual(recognize(element), {error: 'CONTROL.BAD_PROVIDER_OUTPUT'});
  canvas.size = () => items.length;
  assert.equal(publisher.publish(), true);
  publisher.dispose();
  publisher.dispose();
  assert.equal(handlers.size, 0);
  assert.equal(publisher.publish(), false);
  assert.equal(recognize(element), null);
});

test('real Fabric StaticCanvas/Rect: rotation, viewport zoom/pan, clipped move, removal, export and detach',
  {skip: process.env.AAD_TEST_FABRIC_MODULE ? false : 'set AAD_TEST_FABRIC_MODULE to a local Fabric browser ESM bundle'}, async () => {
    const {StaticCanvas, Rect, config, setEnv} = await import(pathToFileURL(process.env.AAD_TEST_FABRIC_MODULE));
    class MemoryCanvas extends CanvasElement {
      style = {};
      classList = {add() {}, remove() {}};
      ctx = Object.fromEntries('clearRect save restore transform scale beginPath moveTo lineTo closePath fill'.split(' ').map(key => [key, () => {}]));
      getContext() { return this.ctx; }
      setAttribute(key, value) {
        super.setAttribute(key, value);
        if (key === 'width' || key === 'height') this[key] = Number(value);
      }
    }
    setEnv({window: {devicePixelRatio: 2}, document: {createElement: () => new MemoryCanvas()}, dispose() {}});
    config.configure({devicePixelRatio: 2});
    const element = new MemoryCanvas();
    const canvas = new StaticCanvas(element, {width: 200, height: 100, renderOnAddRemove: false});
    assert.equal(element.width, 400);
    const options = {originX: 'center', originY: 'center', width: 20, height: 10, strokeWidth: 0, objectCaching: false};
    const rect = new Rect({...options, left: 40, top: 30, angle: 90}), other = new Rect({...options, left: 60, top: 40});
    canvas.add(rect, other);
    const publisher = attachFabricScene(canvas, {describe: object => ({id: object === rect ? 'r' : 's', label: 'sample'}),
      getEdges: () => [{from: rect, to: other}]});
    try {
      const read = () => recognize(element);
      checkBox(read().objects[0].box, [0.175, 0.2, 0.05, 0.2]);
      assert.deepEqual(read().edges, [{from: 'r', to: 's', status: 'confirmed'}]);
      canvas.setViewportTransform([2, 0, 0, 2, -20, -30]);
      canvas.renderAll();
      checkBox(read().objects[0].box, [0.25, 0.1, 0.1, 0.4]);
      const beforeExport = read();
      canvas.toCanvasElement(2, {left: 10, width: 50});
      assert.deepEqual(read(), beforeExport);
      rect.set({left: 0, angle: 0, scaleX: 2});
      rect.setCoords();
      canvas.renderAll();
      checkBox(read().objects[0].box, [0, 0.2, 0.1, 0.2]);
      canvas.remove(rect);
      assert.deepEqual(read().edges, []);
      canvas.renderAll();
      assert.deepEqual(read().objects.map(object => object.id), ['s']);
      assert.deepEqual(read().edges, []);
      publisher.dispose();
      canvas.renderAll();
      assert.equal(read(), null);
    } finally {
      publisher.dispose();
      await canvas.dispose();
    }
  });
