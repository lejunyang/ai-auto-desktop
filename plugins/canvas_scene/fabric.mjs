const attribute = 'data-aad-scene';
const token = value => typeof value === 'string' && /^[A-Za-z0-9_][A-Za-z0-9_.:-]{0,63}$/.test(value);

function requireScene(condition) {
  if (!condition) throw new Error('Invalid Fabric scene');
}

function clip(points, axis, bound, direction) {
  const result = [];
  let previous = points.at(-1);
  for (const current of points) {
    const inside = direction * (current[axis] - bound) >= 0;
    if (inside !== (direction * (previous[axis] - bound) >= 0)) {
      const fraction = (bound - previous[axis]) / (current[axis] - previous[axis]);
      const other = 1 - axis;
      const crossing = [...current];
      crossing[axis] = bound;
      crossing[other] = previous[other] + fraction * (current[other] - previous[other]);
      requireScene(Number.isFinite(crossing[other]));
      result.push(crossing);
    }
    if (inside) result.push(current);
    previous = current;
  }
  return result;
}

function viewportBox(object, matrix, width, height) {
  const [scaleX, skewY, skewX, scaleY, offsetX, offsetY] = matrix;
  const corners = object.getCoords();
  requireScene(Array.isArray(corners) && corners.length === 4);
  let points = corners.map(({x, y}) => {
    const point = [(scaleX * x + skewX * y + offsetX) / width, (skewY * x + scaleY * y + offsetY) / height];
    requireScene(Number.isFinite(x) && Number.isFinite(y) && point.every(Number.isFinite));
    return point;
  });
  for (const axis of [0, 1]) points = clip(clip(points, axis, 0, 1), axis, 1, -1);
  if (points.length < 3) return null;
  const horizontal = points.map(point => point[0]), vertical = points.map(point => point[1]);
  const fx = Math.min(...horizontal), fy = Math.min(...vertical);
  const fw = Math.max(...horizontal) - fx, fh = Math.max(...vertical) - fy;
  return fw > 0 && fh > 0 ? {fx, fy, fw, fh} : null;
}

export function attachFabricScene(canvas, {describe = () => ({}), getEdges = () => []} = {}) {
  const target = canvas.upperCanvasEl ?? canvas.getElement();
  if (target.hasAttribute(attribute)) throw new Error('Fabric canvas already has a scene publisher');
  const generated = new WeakMap();
  let nextId = 1, active = true;

  function encode() {
    const width = canvas.getWidth(), height = canvas.getHeight(), matrix = canvas.viewportTransform;
    requireScene(Number.isFinite(width) && width > 0 && Number.isFinite(height) && height > 0 &&
      Array.isArray(matrix) && matrix.length === 6 && matrix.every(Number.isFinite) &&
      canvas.size() <= 256);
    const ids = new Set(), published = new Map(), objects = [], edges = [];
    for (const object of canvas.getObjects()) {
      if (!object.visible || object.opacity === 0) continue;
      const box = viewportBox(object, matrix, width, height);
      if (!box) continue;
      const metadata = describe(object);
      if (metadata === null) continue;
      let {id, kind = object.type, label = ''} = metadata;
      if (id === undefined) {
        if (!generated.has(object)) generated.set(object, `fabric-${nextId++}`);
        id = generated.get(object);
      }
      requireScene(token(id) && !ids.has(id) && token(kind) && typeof label === 'string' &&
        label.length <= 256 && !/[\p{Cc}\p{Cs}]/u.test(label));
      ids.add(id);
      published.set(object, id);
      objects.push({id, kind, label, box});
    }
    const links = getEdges();
    requireScene(Array.isArray(links) && links.length <= 512);
    const pairs = new Set();
    for (const link of links) {
      requireScene(link?.from && typeof link.from === 'object' && link.to && typeof link.to === 'object');
      if (!published.has(link.from) || !published.has(link.to)) continue;
      const from = published.get(link.from), to = published.get(link.to), pair = from + '\0' + to;
      requireScene(!pairs.has(pair));
      pairs.add(pair);
      edges.push({from, to});
    }
    const raw = JSON.stringify({objects, edges});
    requireScene(raw.length <= 65_536 && new TextEncoder().encode(raw).length <= 65_536);
    return raw;
  }

  function publish() {
    if (!active) return false;
    let raw;
    try { raw = encode(); } catch { raw = '{}'; }
    if (target.getAttribute(attribute) !== raw) target.setAttribute(attribute, raw);
    return raw !== '{}';
  }
  const stops = [canvas.on('after:render', ({ctx}) => { if (ctx === canvas.getContext()) publish(); }),
    ...['object:added', 'object:removed', 'object:modified'].map(event => canvas.on(event, publish))];
  publish();
  return {target, publish, dispose() {
    if (!active) return;
    active = false;
    for (const stop of stops) stop();
    target.removeAttribute(attribute);
  }};
}
