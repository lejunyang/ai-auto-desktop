/**
 * Canvas editor integration v1: set data-aad-scene to a UTF-8 JSON object
 * {objects:[{id,kind,label,box:{fx,fy,fw,fh}}],edges:[{from,to}]}. Fractions
 * describe the canvas-local left, top, width and height (positive area, inside
 * the unit square). IDs are unique; edges are unique, directed ID pairs.
 * A producer may publish an empty scene. Unknown or repeated keys are invalid.
 * Limit: 64 KiB, 256 objects, 512 edges. IDs and kinds are 1–64 ASCII token
 * characters (see token below); labels are 0–256 UTF-16 units of Unicode text
 * without control characters. Strings are data, never selectors or URLs.
 * Absence on a verified canvas returns null; invalid scope or data fails closed.
 * Confirmed means explicitly reported by the editor, not verified from pixels.
 */
(args => {
  const invalid = {error: 'CONTROL.BAD_PROVIDER_OUTPUT'};
  try {
    const canvas = globalThis.__aadControlV1.scope_element(args);
    if (!(canvas instanceof HTMLCanvasElement)) return invalid;
    const raw = HTMLCanvasElement.prototype.getAttribute.call(canvas, 'data-aad-scene');
    if (raw === null) return null;
    if (raw.length > 65_536 || new TextEncoder().encode(raw).length > 65_536) return invalid;

    let quoted = false, depth = 0, members = 0;
    for (let index = 0; index < raw.length; index++) {
      const char = raw[index];
      if (quoted && char === '\\') index++;
      else if (char === '"') quoted = !quoted;
      else if (!quoted) {
        if (char === ':') members++;
        else if (char === '[' || char === '{') {
          if (++depth > 4) return invalid;
        } else if (char === ']' || char === '}') depth--;
      }
    }

    const scene = JSON.parse(raw);
    function exact(value, keys) {
      if (!value || typeof value !== 'object' || Array.isArray(value) ||
          Object.keys(value).length !== keys.length || !keys.every(key => Object.hasOwn(value, key))) return false;
      members -= keys.length;
      return true;
    }
    if (!exact(scene, ['objects', 'edges']) || !Array.isArray(scene.objects) ||
        !Array.isArray(scene.edges) || scene.objects.length > 256 || scene.edges.length > 512) return invalid;

    const token = value => typeof value === 'string' && /^[A-Za-z0-9_][A-Za-z0-9_.:-]{0,63}$/.test(value);
    const axis = (start, size) => Number.isFinite(start) && Number.isFinite(size) &&
      start >= 0 && size > 0 && start + size > start && start + size <= 1;
    const ids = new Set(), pairs = new Set(), objects = [], edges = [];
    for (const object of scene.objects) {
      if (!exact(object, ['id', 'kind', 'label', 'box'])) return invalid;
      const {id, kind, label, box} = object;
      if (!token(id) || ids.has(id) || !token(kind) || typeof label !== 'string' ||
          label.length > 256 || /[\p{Cc}\p{Cs}]/u.test(label) || !exact(box, ['fx', 'fy', 'fw', 'fh'])) return invalid;
      const {fx, fy, fw, fh} = box;
      if (!axis(fx, fw) || !axis(fy, fh)) return invalid;
      ids.add(id);
      objects.push({id, kind, label, box: {fx, fy, fw, fh}, status: 'confirmed'});
    }
    for (const edge of scene.edges) {
      if (!exact(edge, ['from', 'to'])) return invalid;
      const {from, to} = edge;
      if (!ids.has(from) || !ids.has(to)) return invalid;
      const pair = from + '\0' + to;
      if (pairs.has(pair)) return invalid;
      pairs.add(pair);
      edges.push({from, to, status: 'confirmed'});
    }
    if (members !== 0) return invalid;
    return {source: 'data-aad-scene', status: 'confirmed', objects, edges};
  } catch {
    return invalid;
  }
})
