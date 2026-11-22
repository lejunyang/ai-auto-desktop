import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import test from 'node:test';
import {Script} from 'node:vm';

const expression = readFileSync(new URL('../../crates/aad-control/src/browser/editor_scene.js', import.meta.url), 'utf8');
const args = {document_token: 'doc', observation: 'snapshot', node_id: 'canvas'};
const script = new Script(`${expression}(${JSON.stringify(args)})`);
const invalid = {error: 'CONTROL.BAD_PROVIDER_OUTPUT'};

class Canvas {
  #raw;
  constructor(raw) { this.#raw = raw; }
  getAttribute(name) {
    assert.equal(name, 'data-aad-scene');
    return this.#raw;
  }
}

function read(target) {
  const observations = [];
  const result = script.runInNewContext({HTMLCanvasElement: Canvas, TextEncoder, __aadControlV1: {
    scope_element(received) { observations.push(structuredClone(received)); return target; },
  }}, {codeGeneration: {strings: false, wasm: false}});
  assert.deepEqual(observations, [args]);
  return structuredClone(result);
}
const parse = raw => read(new Canvas(raw));
const inspect = value => parse(JSON.stringify(value));
const shape = (id, extra = {}) => ({id, kind: 'shape:box', label: '', box: {fx: 0, fy: 0.25, fw: 1, fh: 0.75}, ...extra});
const scene = (objects = [], edges = []) => ({objects, edges});
const confirmed = item => ({...item, status: 'confirmed'});

test('only a revalidated canvas without the attribute counts as absent', () => {
  assert.deepEqual(read(null), invalid);
  assert.deepEqual(read({getAttribute() { return null; }}), invalid);
  assert.equal(parse(null), null);
  assert.deepEqual(parse(''), invalid);
  assert.deepEqual(inspect(scene()), {source: 'data-aad-scene', status: 'confirmed', objects: [], edges: []});
});

test('returns declared objects and directed edges in order without executing labels or page methods', () => {
  const objects = [shape('__proto__', {label: '画布🟦 "a:b" \\ :{[ https://example.invalid/'}), shape('node.1')];
  const edges = [{from: 'node.1', to: '__proto__'}, {from: '__proto__', to: 'node.1'}, {from: 'node.1', to: 'node.1'}];
  const canvas = new Canvas(JSON.stringify(scene(objects, edges)));
  canvas.getAttribute = () => { throw Error('page method'); };
  assert.deepEqual(read(canvas), {source: 'data-aad-scene', status: 'confirmed',
    objects: objects.map(confirmed), edges: edges.map(confirmed)});
});

test('rejects malformed, ambiguous, or extended inputs rather than exporting partial graphs', () => {
  const ordinary = shape('a');
  for (const raw of ['null', '[]', '{}', '{"objects":[]}', '{"objects":[],"edges":[],}',
    '{"objects":{},"edges":[]}', '{"objects":[],"edges":{},"edges":[]}',
    '{"objects":[],"edges":[],"__proto__":{}}', 'globalThis.makeScene()', '"https://example.invalid/model"',
    JSON.stringify(scene([ordinary])).replace('"fx":0', '"f\\u0078":2,"fx":0'),
    JSON.stringify(scene([ordinary])).replace('"fw":1', '"fw":1e400'),
    '['.repeat(10_000) + ']'.repeat(10_000)]) assert.deepEqual(parse(raw), invalid, raw.slice(0, 100));

  for (const object of [null, [], {...ordinary, href: 'https://example.invalid'}, {...ordinary, kind: undefined},
    {...ordinary, label: 0}, {...ordinary, label: '\ud800'}, {...ordinary, label: 'a\0b'},
    {...ordinary, id: ''}, {...ordinary, id: 'a/b'}, {...ordinary, kind: 'an invalid kind'},
    {...ordinary, box: null}, {...ordinary, box: {...ordinary.box, status: 'confirmed'}}]) {
    assert.deepEqual(inspect(scene([shape('first'), object])), invalid);
  }
  assert.deepEqual(inspect(scene([ordinary, ordinary])), invalid);
  const edge = {from: 'a', to: 'a'};
  for (const edges of [[null], [{to: 'a'}], [{from: 'a', to: 1}], [{...edge, to: 'missing'}],
    [{...edge, status: 'confirmed'}], [edge, edge]]) assert.deepEqual(inspect(scene([ordinary], edges)), invalid);
});

test('boxes must have finite numeric fractions and positive contained area', () => {
  for (const bad of [{fx: -0.1}, {fy: '0'}, {fx: 1, fw: 1e-20}, {fw: 0}, {fh: -0.25},
    {fx: 0.9, fw: 0.2}, {fy: 0.5, fh: 0.6}, {fw: null}, {fh: undefined}]) {
    const object = shape('node');
    Object.assign(object.box, bad);
    assert.deepEqual(inspect(scene([object])), invalid, JSON.stringify(bad));
  }
});

test('enforces byte, string, object and edge limits at their boundaries', () => {
  const object = shape('a'.repeat(64), {kind: 'k'.repeat(64), label: '🟦'.repeat(128)});
  const raw = JSON.stringify(scene([object]));
  const padded = raw + ' '.repeat(65_536 - Buffer.byteLength(raw));
  assert.deepEqual(parse(padded).objects, [confirmed(object)]);
  assert.deepEqual(parse(padded + ' '), invalid);
  for (const key of ['id', 'kind', 'label']) assert.deepEqual(inspect(scene([{...object, [key]: object[key] + 'x'}])), invalid);

  const objects = Array.from({length: 256}, (_, index) => shape(String(index)));
  const edges = objects.flatMap(({id}, index) => [1, 2].map(offset => ({from: id, to: String((index + offset) % objects.length)})));
  assert.equal(inspect(scene(objects, edges)).edges.length, 512);
  assert.deepEqual(inspect(scene([...objects, shape('extra')], edges)), invalid);
  assert.deepEqual(inspect(scene(objects, [...edges, {from: '0', to: '0'}])), invalid);
});
