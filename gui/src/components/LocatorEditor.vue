<script setup lang="ts">
/**
 * Correct a step's locator, and see what it selects before keeping it.
 *
 * The trying is not a convenience. A locator that matches the wrong element is
 * indistinguishable from a correct one until it runs: `{"role": "button",
 * "nth": 3}` on a plain WinForms window selects the title bar's close button,
 * because title-bar buttons share the tree and sit higher on the screen. Showing
 * only "found" would let someone keep that and discover it at replay.
 */
import { computed, ref, watch } from "vue";
import { asFailure, bridge, type Locator, type WindowInfo } from "../bridge";
import {
  DIRECTIONS,
  REQUIRABLE_STATES,
  countsAcrossWindow,
  describe,
  draftProblem,
  fromDraft,
  isBeyondForm,
  toDraft,
  type LocatorDraft,
} from "../locator";

const props = defineProps<{
  locator: Locator | null;
  /** The live window to try against, or null when none is selected. */
  window: WindowInfo | null;
}>();

const emit = defineEmits<{ apply: [Locator]; close: [] }>();

const draft = ref<LocatorDraft>(toDraft(props.locator));
// Starts in whichever view can actually show this locator. Putting a nested
// anchor into the form would drop the part the form cannot see.
const raw = ref(isBeyondForm(props.locator));
const rawText = ref(JSON.stringify(props.locator ?? {}, null, 2));

interface Trial {
  matched: number;
  /** What it selected, when it selected exactly one. */
  summary?: string;
  /** The competing elements, when it selected several. */
  candidates?: string[];
  error?: string;
}

const trial = ref<Trial | null>(null);
const trying = ref(false);

/** The locator the current view describes, or null when it is not valid. */
const candidate = computed<Locator | null>(() => {
  if (raw.value) {
    try {
      const parsed = JSON.parse(rawText.value);
      return parsed && typeof parsed === "object" ? (parsed as Locator) : null;
    } catch {
      return null;
    }
  }
  return draftProblem(draft.value) ? null : fromDraft(draft.value);
});

const problem = computed(() => {
  if (raw.value) {
    try {
      const parsed = JSON.parse(rawText.value);
      if (!parsed || typeof parsed !== "object") {
        return "a locator is a JSON object";
      }
      return Object.keys(parsed).length ? null : "a locator has to constrain something";
    } catch (error) {
      return `not valid JSON: ${(error as Error).message}`;
    }
  }
  return draftProblem(draft.value);
});

const preview = computed(() => (candidate.value ? describe(candidate.value) : ""));

// Asked of the locator itself rather than of the fields, so the JSON view and
// the form give the same answer. Two implementations would drift, and the one
// that drifts is the one nobody looks at.
const counting = computed(() => countsAcrossWindow(candidate.value));

// A trial describes the locator as it was when it ran, so it stops being true
// the moment anything changes. Keeping it on screen would show a stale
// confirmation beside an edited locator.
watch([draft, rawText, raw], () => {
  trial.value = null;
}, { deep: true });

/** Move the current locator into the other view rather than starting over. */
function toggleView(): void {
  if (raw.value) {
    const parsed = candidate.value;
    if (parsed && !isBeyondForm(parsed)) {
      draft.value = toDraft(parsed);
      raw.value = false;
    }
    return;
  }
  rawText.value = JSON.stringify(candidate.value ?? {}, null, 2);
  raw.value = true;
}

const canLeaveRaw = computed(
  () => raw.value && candidate.value !== null && !isBeyondForm(candidate.value),
);

async function tryIt(): Promise<void> {
  const locator = candidate.value;
  if (!locator || !props.window) {
    return;
  }
  trying.value = true;
  try {
    const found = await bridge.tryLocator(props.window.window_id, locator);
    const summary = found.node?.summary;
    trial.value = found.found
      ? {
          matched: 1,
          summary: typeof summary === "string" ? summary : summarise(found.node),
        }
      : { matched: 0 };
  } catch (error) {
    const failure = asFailure(error);
    if (failure.code === "DRIVER.AMBIGUOUS_MATCH") {
      const details = (failure.details ?? {}) as Record<string, unknown>;
      const candidates = (details.candidates ?? []) as { summary?: string }[];
      trial.value = {
        matched: Number(details.match_count ?? candidates.length),
        // The competition is what tells someone how to narrow it, so it is shown
        // rather than reduced to a count.
        candidates: candidates.map((entry) => entry.summary ?? "an element"),
      };
    } else {
      trial.value = { matched: 0, error: failure.message };
    }
  }
  trying.value = false;
}

/** A readable line for a node the driver returned without one. */
function summarise(node: Record<string, unknown> | undefined): string {
  if (!node) {
    return "an element";
  }
  const parts = [String(node.role ?? "element")];
  if (node.name) {
    parts.push(JSON.stringify(node.name));
  }
  return parts.join(" ");
}

function apply(): void {
  const locator = candidate.value;
  if (locator) {
    emit("apply", locator);
  }
}
</script>

<template>
  <div class="editor">
    <header>
      <strong>Locator</strong>
      <span class="muted">{{ raw ? "JSON" : "fields" }}</span>
      <span class="spacer"></span>
      <button
        @click="toggleView"
        :disabled="raw && !canLeaveRaw"
        :title="raw && !canLeaveRaw
          ? 'This locator says more than the fields can show'
          : 'Switch view'"
      >
        {{ raw ? "Use fields" : "Edit as JSON" }}
      </button>
      <button @click="emit('close')">Close</button>
    </header>

    <div class="body" v-if="!raw">
      <div class="grid">
        <label><span>role</span><input v-model="draft.role" placeholder="button, edit…" /></label>
        <label><span>name</span><input v-model="draft.name" placeholder="the label or text" /></label>
        <label><span>automation id</span><input v-model="draft.automationId" /></label>
        <label><span>class</span><input v-model="draft.className" /></label>
        <label>
          <span>must be</span>
          <select v-model="draft.requireState">
            <option value="">anything</option>
            <option v-for="state in REQUIRABLE_STATES" :key="state" :value="state">
              {{ state }}
            </option>
          </select>
        </label>
        <label
          title="Counted through the whole window, frame included: the first
button is often Minimise, and in a browser the count runs through the toolbar
before reaching the page. Add a `next to` anchor to count within a region."
        >
          <span>position</span>
          <input v-model="draft.nth" placeholder="1, 2, 3… or last" />
        </label>
      </div>

      <fieldset>
        <legend>next to</legend>
        <div class="grid">
          <label><span>element named</span><input v-model="draft.nearName" placeholder="Name:" /></label>
          <label><span>of role</span><input v-model="draft.nearRole" placeholder="text" /></label>
          <label>
            <span>direction</span>
            <select v-model="draft.direction">
              <option v-for="value in DIRECTIONS" :key="value" :value="value">{{ value }}</option>
            </select>
          </label>
          <label><span>within px</span><input v-model="draft.nearWithin" placeholder="40" /></label>
        </div>

        <div class="group">
          <p class="hint">
            Or search inside one container. Useful when several elements share a
            name and only their panel differs — measured here, naming the
            container cut same-role siblings from a median of 66 to 3.
          </p>
          <label>
            <span>inside element named</span>
            <input v-model="draft.containerName" placeholder="Terminal actions" />
          </label>
          <label>
            <span>of role</span>
            <input v-model="draft.containerRole" placeholder="tool_bar" />
          </label>
        </div>
      </fieldset>
    </div>

    <div class="body" v-else>
      <textarea v-model="rawText" class="mono" rows="9" spellcheck="false"></textarea>
      <p class="muted small">
        The fields cannot show every locator — a nested anchor or a state that must
        be false lives here.
      </p>
    </div>

    <p class="preview" v-if="preview">selects {{ preview }}</p>
    <p class="hint" v-if="counting">
      Counting spans the whole window, frame included — try it before keeping it.
    </p>
    <p class="problem" v-if="problem">{{ problem }}</p>

    <div class="trial" v-if="trial">
      <template v-if="trial.error">
        <span class="problem">{{ trial.error }}</span>
      </template>
      <template v-else-if="trial.matched === 1">
        <span class="ok">✓ matched</span>
        <span class="mono">{{ trial.summary }}</span>
      </template>
      <template v-else-if="trial.matched === 0">
        <span class="problem">✗ matched nothing in this window</span>
      </template>
      <template v-else>
        <span class="problem">✗ matched {{ trial.matched }} elements — narrow it further</span>
        <ul>
          <li v-for="(entry, index) in trial.candidates" :key="index" class="mono">
            {{ entry }}
          </li>
        </ul>
      </template>
    </div>

    <footer>
      <button
        @click="tryIt"
        :disabled="trying || !candidate || !props.window"
        :title="props.window ? 'Try it against ' + props.window.title : 'Select a window to try'"
      >
        {{ trying ? "Trying…" : "Try it" }}
      </button>
      <span class="spacer"></span>
      <button class="primary" @click="apply" :disabled="!candidate">Use this</button>
    </footer>
  </div>
</template>

<style scoped>
.editor {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 10px;
  border: 1px solid var(--line);
  border-radius: 8px;
  background: var(--panel);
}

header,
footer {
  display: flex;
  align-items: center;
  gap: 8px;
}

.spacer {
  flex: 1;
}

.grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 6px;
}

label {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
}

label span {
  width: 88px;
  color: var(--muted);
  flex: none;
}

label input,
label select {
  flex: 1;
  min-width: 0;
}

fieldset {
  margin: 0;
  padding: 6px 8px 8px;
  border: 1px solid var(--line);
  border-radius: 6px;
}

legend {
  padding: 0 4px;
  font-size: 11px;
  color: var(--muted);
}

textarea {
  width: 100%;
  resize: vertical;
}

.preview {
  margin: 0;
  font-size: 12px;
  color: var(--muted);
}

.problem {
  margin: 0;
  font-size: 12px;
  color: #e0a34b;
}

.hint {
  margin: 0;
  font-size: 11px;
  color: var(--muted);
}

.ok {
  color: #57b96a;
}

.small {
  font-size: 11px;
}

.trial {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 6px 8px;
  border-radius: 6px;
  background: rgba(255, 255, 255, 0.04);
  font-size: 12px;
}

.trial ul {
  margin: 0;
  padding-left: 16px;
}
</style>
