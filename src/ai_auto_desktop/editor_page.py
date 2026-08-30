"""The editor page: four views over one recording.

Plain HTML and JavaScript with no build step and no external assets -- the page
must load with the machine offline, and a bundler would add a toolchain to a
project whose only dependencies are PyYAML, jsonschema and Pillow.

Native HTML5 drag-and-drop rather than a library, for the same reason.

Every mutation round-trips through the server and the page re-renders from the
response.  It never predicts the outcome locally: an edit can be refused, and a
UI that had already moved the row would show a state the recording is not in.
"""

PAGE = r"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Recording editor</title>
<style>
  :root {
    --line: #d7dae0; --ink: #1c1f24; --muted: #6b7280;
    --warn: #b45309; --warn-bg: #fef3c7;
    --bad: #b91c1c; --bad-bg: #fee2e2;
    --good: #15803d; --good-bg: #dcfce7;
    --sel: #1d4ed8;
  }
  * { box-sizing: border-box; }
  body {
    margin: 0; font: 13px/1.5 "Segoe UI", system-ui, sans-serif;
    color: var(--ink); background: #f6f7f9;
  }
  header {
    padding: 10px 16px; background: #fff; border-bottom: 1px solid var(--line);
    display: flex; gap: 16px; align-items: baseline;
  }
  header h1 { font-size: 14px; margin: 0; font-weight: 600; }
  header .meta { color: var(--muted); font-size: 12px; }
  header button { margin-left: auto; }
  main { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; padding: 12px; }
  section {
    background: #fff; border: 1px solid var(--line); border-radius: 6px;
    display: flex; flex-direction: column; min-height: 220px;
  }
  section > h2 {
    font-size: 12px; text-transform: uppercase; letter-spacing: .04em;
    color: var(--muted); margin: 0; padding: 8px 12px;
    border-bottom: 1px solid var(--line);
  }
  section > div { padding: 10px 12px; overflow: auto; }
  button {
    font: inherit; padding: 3px 9px; border: 1px solid var(--line);
    background: #fff; border-radius: 4px; cursor: pointer;
  }
  button:hover { background: #f0f1f3; }
  button:disabled { opacity: .45; cursor: default; }
  input[type=text] {
    font: inherit; padding: 3px 6px; border: 1px solid var(--line);
    border-radius: 4px; width: 100%;
  }
  ol { list-style: none; margin: 0; padding: 0; }
  li.step {
    border: 1px solid var(--line); border-radius: 4px; margin-bottom: 5px;
    padding: 6px 8px; background: #fff; cursor: grab;
    display: flex; gap: 8px; align-items: center;
  }
  li.step.nested { margin-left: 22px; border-left: 3px solid #cbd5e1; }
  li.step.disabled { opacity: .5; background: #f8f9fa; }
  li.step.selected { border-color: var(--sel); box-shadow: 0 0 0 1px var(--sel); }
  li.step.dragging { opacity: .35; }
  li.step.dropbefore { border-top: 2px solid var(--sel); }
  .grip { color: #9ca3af; cursor: grab; user-select: none; }
  .sid { font-weight: 600; }
  .kind {
    font-size: 11px; color: var(--muted); border: 1px solid var(--line);
    border-radius: 3px; padding: 0 4px;
  }
  .spacer { margin-left: auto; }
  .tag { font-size: 11px; border-radius: 3px; padding: 0 5px; }
  .tag.unique { background: var(--good-bg); color: var(--good); }
  .tag.unresolved { background: var(--bad-bg); color: var(--bad); }
  .banner { padding: 6px 9px; border-radius: 4px; margin-bottom: 6px; font-size: 12px; }
  .banner.warn { background: var(--warn-bg); color: var(--warn); }
  .banner.bad  { background: var(--bad-bg);  color: var(--bad); }
  .banner.good { background: var(--good-bg); color: var(--good); }
  dl { margin: 0; display: grid; grid-template-columns: auto 1fr; gap: 3px 10px; }
  dt { color: var(--muted); }
  dd { margin: 0; font-family: Consolas, monospace; word-break: break-all; }
  .empty { color: var(--muted); font-style: italic; }
  .row { display: flex; gap: 6px; align-items: center; margin-bottom: 6px; }
  code { font-family: Consolas, monospace; background: #f1f3f5; padding: 0 3px; }
</style>
</head>
<body>
<header>
  <h1>Recording editor</h1>
  <span class="meta" id="meta"></span>
  <button id="undo">Undo</button>
</header>

<main>
  <section style="grid-row: span 2">
    <h2>Steps &mdash; drag to reorder</h2>
    <div>
      <div id="orderWarnings"></div>
      <ol id="steps"></ol>
    </div>
  </section>

  <section>
    <h2>Step detail</h2>
    <div id="detail"><p class="empty">Select a step.</p></div>
  </section>

  <section>
    <h2>Logic &amp; inputs</h2>
    <div>
      <div class="row">
        <input type="text" id="logicId" placeholder="condition step id">
        <input type="text" id="logicWhen" placeholder="${{ inputs.name }}">
        <button id="addLogic">Wrap selected</button>
      </div>
      <div class="row">
        <input type="text" id="inputName" placeholder="input name">
        <button id="addInput">Declare input</button>
      </div>
      <div id="inputs"></div>
    </div>
  </section>

  <section style="grid-column: span 2">
    <h2>Compile result</h2>
    <div id="compile"></div>
  </section>
</main>

<script>
// Injected by the server when the page is served.  It is not fetched from
// an endpoint: an unauthenticated endpoint that returns the token lets any
// local process take it, which was measured working before this changed.
let TOKEN = "__RECORDER_TOKEN__";
let STATE = null;
let SELECTED = null;

async function call(path, body) {
  const options = {
    headers: {"X-Recorder-Token": TOKEN, "Content-Type": "application/json"}
  };
  if (body !== undefined) {
    options.method = "POST";
    options.body = JSON.stringify(body);
  }
  const response = await fetch(path, options);
  const payload = await response.json();
  if (!response.ok) {
    // Surfaced rather than swallowed: a refused edit is information the
    // operator needs, and the recording is unchanged when it happens.
    showError(payload.error);
    return null;
  }
  return payload;
}

function showError(error) {
  const box = document.getElementById("compile");
  const message = error && (error.message || error.error || JSON.stringify(error));
  const code = error && error.code ? error.code + ": " : "";
  box.innerHTML = '<div class="banner bad">Edit refused &mdash; ' +
                  escapeHtml(code + message) + '</div>' + box.innerHTML;
}

function escapeHtml(value) {
  return String(value === undefined || value === null ? "" : value)
    .replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function render(state) {
  if (!state) return;
  STATE = state;
  document.getElementById("meta").textContent =
    (state.name || "(unnamed)") + "  \u00b7  window " +
    JSON.stringify(state.window || {});

  renderSteps(state);
  renderDetail(state);
  renderInputs(state);
  renderCompile(state);
}

function renderSteps(state) {
  const warnBox = document.getElementById("orderWarnings");
  warnBox.innerHTML = (state.warnings || []).map(function (w) {
    return '<div class="banner warn">' + escapeHtml(w.message) +
           (w.code === "ORDER_DIVERGED"
             ? " (recorded: " + escapeHtml((w.recorded || []).join(" \u2192 ")) + ")"
             : "") + '</div>';
  }).join("");

  const list = document.getElementById("steps");
  list.innerHTML = "";
  state.steps.forEach(function (step) {
    const item = document.createElement("li");
    item.className = "step" + (step.depth ? " nested" : "") +
                     (step.enabled ? "" : " disabled") +
                     (step.id === SELECTED ? " selected" : "");
    // Only top-level steps are draggable: reordering a logic child across
    // containers would change which branch it belongs to, which is a different
    // operation from moving it within the flow.
    item.draggable = step.depth === 0;
    item.dataset.id = step.id;

    const strategy = step.kind === "interaction"
      ? '<span class="tag ' + (step.verified ? "unique" : "unresolved") + '">' +
        escapeHtml(step.strategy || "unresolved") + '</span>'
      : "";

    item.innerHTML =
      (step.depth === 0 ? '<span class="grip">\u2630</span>' : '<span></span>') +
      '<span class="sid">' + escapeHtml(step.id) + '</span>' +
      '<span class="kind">' + escapeHtml(step.kind) +
        (step.action ? " \u00b7 " + escapeHtml(step.action) : "") + '</span>' +
      strategy +
      '<span class="spacer"></span>' +
      '<button data-toggle="' + escapeHtml(step.id) + '">' +
        (step.enabled ? "Disable" : "Enable") + '</button>';

    item.addEventListener("click", function (event) {
      if (event.target.dataset.toggle) return;
      SELECTED = step.id;
      render(STATE);
    });
    list.appendChild(item);
  });

  list.querySelectorAll("[data-toggle]").forEach(function (button) {
    button.addEventListener("click", async function (event) {
      event.stopPropagation();
      const id = button.dataset.toggle;
      const step = STATE.steps.find(function (s) { return s.id === id; });
      render(await call("/api/enable", {id: id, enabled: !step.enabled}));
    });
  });

  wireDragAndDrop(list);
}

function wireDragAndDrop(list) {
  let dragged = null;

  list.addEventListener("dragstart", function (event) {
    const item = event.target.closest("li.step");
    if (!item || !item.draggable) return;
    dragged = item;
    item.classList.add("dragging");
    event.dataTransfer.effectAllowed = "move";
    // Firefox refuses to start a drag without data set.
    event.dataTransfer.setData("text/plain", item.dataset.id);
  });

  list.addEventListener("dragover", function (event) {
    event.preventDefault();
    const item = event.target.closest("li.step");
    list.querySelectorAll(".dropbefore").forEach(function (el) {
      el.classList.remove("dropbefore");
    });
    if (item && item !== dragged && item.draggable) {
      item.classList.add("dropbefore");
    }
  });

  list.addEventListener("drop", async function (event) {
    event.preventDefault();
    const target = event.target.closest("li.step");
    list.querySelectorAll(".dropbefore").forEach(function (el) {
      el.classList.remove("dropbefore");
    });
    if (!dragged || !target || target === dragged) return;

    const top = STATE.steps.filter(function (s) { return s.depth === 0; })
                           .map(function (s) { return s.id; });
    const from = top.indexOf(dragged.dataset.id);
    const to = top.indexOf(target.dataset.id);
    if (from < 0 || to < 0) return;
    top.splice(to, 0, top.splice(from, 1)[0]);
    dragged = null;
    render(await call("/api/reorder", {order: top}));
  });

  list.addEventListener("dragend", function () {
    list.querySelectorAll(".dragging, .dropbefore").forEach(function (el) {
      el.classList.remove("dragging", "dropbefore");
    });
    dragged = null;
  });
}

function renderDetail(state) {
  const box = document.getElementById("detail");
  const step = state.steps.find(function (s) { return s.id === SELECTED; });
  if (!step) {
    box.innerHTML = '<p class="empty">Select a step.</p>';
    return;
  }

  let html = "";
  if (step.kind === "interaction" && !step.verified) {
    // The driver fails closed on ambiguity, so this has to be visible while
    // editing rather than arriving as DRIVER.AMBIGUOUS during replay.
    html += '<div class="banner bad">Locator not verified unique &mdash; ' +
            'this recording cannot compile until it resolves to exactly one node.</div>';
  }

  html += '<dl>' +
    '<dt>id</dt><dd>' + escapeHtml(step.id) + '</dd>' +
    '<dt>kind</dt><dd>' + escapeHtml(step.kind) +
      (step.action ? " / " + escapeHtml(step.action) : "") + '</dd>' +
    '<dt>enabled</dt><dd>' + step.enabled + '</dd>';
  if (step.of_step) {
    html += '<dt>checks</dt><dd>' + escapeHtml(step.of_step) + '</dd>';
  }
  if (step.logic) {
    html += '<dt>when</dt><dd>' + escapeHtml(JSON.stringify(step.logic)) + '</dd>';
  }
  if (step.locator) {
    html += '<dt>locator</dt><dd>' + escapeHtml(JSON.stringify(step.locator)) + '</dd>';
  }
  if (step.strategy) {
    html += '<dt>strategy</dt><dd>' + escapeHtml(step.strategy) +
            (step.verified ? " (verified)" : " (unverified)") + '</dd>';
  }
  if (step.observed) {
    html += '<dt>observed</dt><dd>' + escapeHtml(JSON.stringify(step.observed)) + '</dd>';
  }
  html += '</dl>';

  if (step.locator) {
    html += '<div class="row" style="margin-top:8px">' +
      '<input type="text" id="locatorEdit" value="' +
      escapeHtml(JSON.stringify(step.locator)) + '">' +
      '<button id="saveLocator">Save</button></div>' +
      '<p class="empty">Editing a locator clears its verification.</p>';
  }
  box.innerHTML = html;

  const save = document.getElementById("saveLocator");
  if (save) {
    save.addEventListener("click", async function () {
      let parsed;
      try {
        parsed = JSON.parse(document.getElementById("locatorEdit").value);
      } catch (err) {
        showError({message: "locator must be valid JSON: " + err.message});
        return;
      }
      render(await call("/api/update", {id: step.id, changes: {locator: parsed}}));
    });
  }
}

function renderInputs(state) {
  const names = Object.keys(state.inputs || {});
  document.getElementById("inputs").innerHTML = names.length
    ? "Declared: " + names.map(function (n) {
        return "<code>" + escapeHtml(n) + "</code>";
      }).join(" ")
    : '<span class="empty">No inputs declared.</span>';
}

function renderCompile(state) {
  const box = document.getElementById("compile");
  const result = state.compile || {};
  if (result.ok) {
    box.innerHTML = '<div class="banner good">Compiles &mdash; ' +
      result.steps.length + ' workflow steps, permissions: ' +
      escapeHtml((result.permissions || []).join(", ")) + '</div>' +
      '<dl><dt>steps</dt><dd>' +
      escapeHtml(result.steps.join(" \u2192 ")) + '</dd></dl>';
  } else {
    const error = result.error || {};
    const where = error.details && error.details.id
      ? ' (step <code>' + escapeHtml(error.details.id) + '</code>)' : "";
    box.innerHTML = '<div class="banner bad">Cannot compile &mdash; ' +
      escapeHtml(error.code || "") + ": " + escapeHtml(error.message || "") +
      where + '</div>';
  }
}

document.getElementById("undo").addEventListener("click", async function () {
  render(await call("/api/undo", {}));
});

document.getElementById("addLogic").addEventListener("click", async function () {
  const id = document.getElementById("logicId").value.trim();
  const when = document.getElementById("logicWhen").value.trim();
  if (!id || !when || !SELECTED) {
    showError({message: "select a step, then give the condition an id and expression"});
    return;
  }
  render(await call("/api/logic", {id: id, when: when, wrap: [SELECTED]}));
});

document.getElementById("addInput").addEventListener("click", async function () {
  const name = document.getElementById("inputName").value.trim();
  if (!name) return;
  render(await call("/api/input", {name: name, spec: {type: "string"}}));
});

(async function start() {
  render(await call("/api/state"));
})();
</script>
</body>
</html>
"""
