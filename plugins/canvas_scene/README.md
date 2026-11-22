# Publish a Fabric.js canvas scene

Import `fabric.mjs` **in the editor application** and pass an existing Fabric `Canvas` or `StaticCanvas`. The adapter has no imports or runtime dependencies. It does not discover editors, read a page global, or fetch scene files. The isolated-world recognizer reads only the `data-aad-scene` attribute on the canvas revalidated from the observation.

```js
import {attachFabricScene} from './fabric.mjs';

// editor is the existing Fabric instance; start and review are Fabric objects.
const metadata = new Map([
  [start, {id: 'start', label: 'Start'}],
  [review, {id: 'review', label: 'Review'}],
]);
const scene = attachFabricScene(editor, {
  describe: object => metadata.get(object) ?? null,
  getEdges: () => [{from: start, to: review}],
});
scene.target.setAttribute('aria-label', 'Expense diagram editor');
```

Use the AAD locator `{"role":"canvas","name":"Expense diagram editor"}`. Give that name to **exactly `scene.target`**, not a wrapper or a second canvas. On interactive Fabric `Canvas`, the target is `editor.upperCanvasEl`: the transparent input layer above the painted canvas. A pointer aimed at the same location on `editor.getElement()` hits the upper layer instead. On `StaticCanvas`, the target is `editor.getElement()`. The adapter writes only to the chosen layer; the interactive upper bitmap alone is **not** a composite screenshot of the diagram.

### What is published and when

- With no callbacks, visible top-level objects get an ID stable for the lifetime of this publisher, Fabric's `object.type` as `kind`, and an empty label. Supply durable editor IDs for use across reloads. `describe` can override any of `id`, `kind`, `label`, or return `null` to omit an object. Publish only information intended to be readable from the DOM; there is no default text or custom-property dump.
- `getEdges` supplies directed pairs of **Fabric object references** from your editor's connection model. A removed, hidden, offscreen, or omitted endpoint drops the edge. A visual line alone does not prove which objects it connects. Groups are top-level objects, not a recursive export of their contents.
- Geometry uses the four scene-plane corners from `object.getCoords()`. The adapter applies the full `viewportTransform` (including pan and zoom), divides by logical `getWidth()` / `getHeight()` rather than retina backing pixels, clips the quadrilateral to the unit square, and reports its axis-aligned bounds. These are editor-declared bounds, not pixel masks or proof of a successful hit. Shadows, clipping paths, other layers and object occlusion are not modeled.
- Publication is synchronous on attach and on Fabric object add/remove/modified events. A completed render of the main context republishes viewport, size, visibility and transform changes; export renders to a different context do not. For programmatic transforms follow Fabric's coordinate-cache contract: `review.set({left: 120, angle: 15}); review.setCoords(); editor.requestRenderAll();`. For pan/zoom: `editor.setViewportTransform([2, 0, 0, 2, -100, -40]); editor.requestRenderAll();`. `editor.renderAll()` updates synchronously if you need to observe immediately. If only labels or edges changed, call `scene.publish()` explicitly. Keep callbacks side-effect free.
- `scene.publish()` returns `true` on success. On oversize or invalid metadata or a throwing callback it returns `false` and replaces any stale graph with `{}` (a rejected payload, **not** a missing integration). Correct the model and republish. The adapter refuses to attach over an existing publisher. Call `scene.dispose()` **before** disposing Fabric: it unsubscribes and removes the attribute; repeated disposal is safe.

### Attribute protocol (also usable without Fabric)

Set `canvas.setAttribute('data-aad-scene', JSON.stringify(graph))` on the **observed target**. Example `graph`:

```json
{"objects":[{"id":"start","kind":"rect","label":"Start","box":{"fx":0.1,"fy":0.2,"fw":0.3,"fh":0.1}}],"edges":[]}
```

Both arrays are required. Each edge has exactly `from` and `to`, naming existing IDs. IDs and directed pairs must be unique; self-loops are valid. IDs/kinds match `[A-Za-z0-9_][A-Za-z0-9_.:-]{0,63}`. Labels may be empty, but must be Unicode scalar text without control characters, at most 256 UTF-16 code units. Boxes are finite numbers: left/top fractions and positive width/height entirely within the canvas. No unknown or repeated fields. Maximum 65,536 UTF-8 bytes, 256 objects, and 512 edges. The Fabric adapter also bounds its input to 256 top-level objects and 512 declared links before filtering; it does not silently truncate.

The isolated expression preserves order and copies the validated graph, adding `source: 'data-aad-scene'` and `status: 'confirmed'` at the root and `status: 'confirmed'` on every object and edge. Confirmed means the editor explicitly declared it, not that pixels corroborate it. No attribute on a revalidated canvas returns `null`; an invalid target or payload returns `{error:'CONTROL.BAD_PROVIDER_OUTPUT'}`. An explicitly empty scene returns a confirmed graph with two empty arrays.

### Standalone verification

Run from the repository root (Node.js). The first two suites run dependency-free; the actual Fabric cases opt in via local executable/module paths. No GUI, install, or network access is required by the tests.

```powershell
node --test tests/browser/editor-scene-unit.test.mjs plugins/canvas_scene/fabric.test.mjs

# Use the browser ESM bundle from the published fabric@6.9.0 npm package.
$env:AAD_TEST_FABRIC_MODULE = '/absolute/path/to/fabric/dist/index.min.mjs'
$env:AAD_TEST_HEADLESS_CHROMIUM = '/absolute/path/to/chromium-or-edge'
node --test plugins/canvas_scene/fabric.test.mjs plugins/canvas_scene/fabric-hit.test.mjs
```

The Node integration test uses actual Fabric `StaticCanvas`/`Rect` geometry, events and removal with a no-pixels memory canvas. The browser test uses actual Fabric `Canvas` in headless Chromium, reads the published bounds through the recognizer, hit-tests their center with `document.elementFromPoint`, then dispatches a synthetic mouse event to verify Fabric picks the expected object. It is not an OS-input or Rust CLI test.
