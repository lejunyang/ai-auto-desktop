<script setup lang="ts">
import { computed, onMounted, reactive, ref, shallowRef } from "vue";
import AppList from "./components/AppList.vue";
import OutlineView from "./components/OutlineView.vue";
import StepList from "./components/StepList.vue";
import FailureBanner from "./components/FailureBanner.vue";
import { asFailure, bridge, type DriverFailure, type Element, type Outline,
  type SavedRecording, type WindowInfo }
  from "./bridge";
import { Recording } from "./recording";

const windows = ref<WindowInfo[]>([]);
const selected = ref<WindowInfo | null>(null);
const outline = shallowRef<Outline | null>(null);
const failure = ref<DriverFailure | null>(null);
const loadingApps = ref(false);
const loadingOutline = ref(false);
const exported = ref<string | null>(null);
const saving = ref(false);
const saved = ref<string | null>(null);
const browsing = ref(false);
const savedRecordings = ref<SavedRecording[]>([]);
const savedDirectory = ref("");

// Reactive so the step list re-renders when the recording mutates in place.
const recording = reactive(new Recording()) as Recording;

const issues = computed(() => recording.validate());

async function guard(work: () => Promise<void>): Promise<void> {
  try {
    await work();
  } catch (error) {
    failure.value = asFailure(error);
  }
}

async function refreshApps(): Promise<void> {
  loadingApps.value = true;
  await guard(async () => {
    windows.value = (await bridge.listApps()).windows;
  });
  loadingApps.value = false;
}

async function select(window: WindowInfo): Promise<void> {
  selected.value = window;
  await readOutline();
}

async function readOutline(): Promise<void> {
  if (!selected.value) {
    return;
  }
  loadingOutline.value = true;
  outline.value = null;
  await guard(async () => {
    outline.value = await bridge.describeWindow(selected.value!.window_id);
  });
  loadingOutline.value = false;
}

function record(element: Element, action: string): void {
  if (!selected.value) {
    return;
  }
  // Pass every open window, so the selector can be checked for uniqueness
  // against its actual competition. Without this a second window of the same
  // application would make the recording refuse to replay.
  recording.add({
    action,
    element,
    window: selected.value,
    openWindows: windows.value,
  });
}

function exportDescriptor(): void {
  exported.value = JSON.stringify(recording.toDescriptor(), null, 2);
}

async function copyExport(): Promise<void> {
  if (exported.value) {
    await navigator.clipboard.writeText(exported.value);
  }
}

/**
 * Write the recording and its compiled workflow to disk.
 *
 * The editable source is saved even when it will not compile: refusing would
 * mean losing work over a step that still needs text typed into it. Only the
 * runnable workflow requires the recording to be valid.
 */
async function save(): Promise<void> {
  saving.value = true;
  saved.value = null;
  await guard(async () => {
    const workflow = recording.canExport ? recording.toDescriptor(recording.name) : {};
    const paths = await bridge.saveRecording(
      recording.name,
      recording.toDocument(),
      workflow,
    );
    saved.value = recording.canExport
      ? `Saved. The workflow is at ${paths.workflow_path}`
      : `Saved the recording, but not a runnable workflow: ${issues.value
          .filter((issue) => issue.blocking)
          .map((issue) => issue.message)
          .join("; ")}`;
    await refreshSaved();
  });
  saving.value = false;
}

async function refreshSaved(): Promise<void> {
  await guard(async () => {
    const listed = await bridge.listRecordings();
    savedRecordings.value = listed.recordings;
    savedDirectory.value = listed.directory;
  });
}

async function openList(): Promise<void> {
  await refreshSaved();
  browsing.value = true;
}

/** Replace the working recording with a saved one. */
async function open(entry: SavedRecording): Promise<void> {
  await guard(async () => {
    const document = await bridge.loadRecording(entry.path);
    const reopened = Recording.fromDocument(document);
    // Mutate in place: the template is bound to this reactive instance, so
    // reassigning the variable would leave the UI showing the old steps.
    recording.steps = reopened.steps;
    recording.name = reopened.name;
    browsing.value = false;
    saved.value = `Opened ${reopened.name}`;
  });
}

onMounted(async () => {
  await refreshApps();
  await refreshSaved();
});
</script>

<template>
  <div class="shell">
    <div class="titlebar">
      <strong>ai-auto-desktop</strong>
      <span class="muted">observe, then act</span>
      <span class="spacer"></span>
      <label class="name">
        <span class="muted">name</span>
        <input v-model="recording.name" spellcheck="false" />
      </label>
      <button @click="save" :disabled="saving || !recording.steps.length">
        {{ saving ? "Saving…" : "Save" }}
      </button>
      <button @click="openList">Open…</button>
    </div>

    <div class="banner" v-if="failure">
      <FailureBanner :failure="failure" @dismiss="failure = null" />
    </div>

    <div class="banner note" v-if="saved">
      <span>{{ saved }}</span>
      <button @click="saved = null">Dismiss</button>
    </div>

    <main>
      <AppList
        :windows="windows"
        :selected="selected?.window_id ?? null"
        :loading="loadingApps"
        @select="select"
        @refresh="refreshApps"
      />

      <OutlineView
        :outline="outline"
        :loading="loadingOutline"
        @record="record"
        @refresh="readOutline"
      />

      <StepList
        :steps="recording.steps"
        :issues="issues"
        @toggle="(id, on) => recording.setEnabled(id, on)"
        @remove="(id) => recording.remove(id)"
        @move="(id, index) => recording.move(id, index)"
        @argument="(id, text) => recording.setArgument(id, text)"
        @assertion="(id, patch) => recording.setAssertion(id, patch)"
        @clear="recording.steps = []"
        @export="exportDescriptor"
      />
    </main>

    <div v-if="exported" class="overlay" @click.self="exported = null">
      <div class="sheet">
        <header>
          <h3>Workflow descriptor</h3>
          <button @click="copyExport">Copy</button>
          <button @click="exported = null">Close</button>
        </header>
        <pre class="mono">{{ exported }}</pre>
      </div>
    </div>

    <div v-if="browsing" class="overlay" @click.self="browsing = false">
      <div class="sheet">
        <header>
          <h3>Open a recording</h3>
          <button @click="refreshSaved">Refresh</button>
          <button @click="browsing = false">Close</button>
        </header>
        <ul class="saved" v-if="savedRecordings.length">
          <li v-for="entry in savedRecordings" :key="entry.path">
            <button class="row" @click="open(entry)">
              <strong>{{ entry.name }}</strong>
              <span class="muted mono">{{ entry.path }}</span>
            </button>
          </li>
        </ul>
        <p v-else class="empty muted">
          Nothing saved yet. Recordings are kept in
          <span class="mono">{{ savedDirectory }}</span>
        </p>
      </div>
    </div>
  </div>
</template>

<style scoped>
.shell {
  display: flex;
  flex-direction: column;
  height: 100vh;
}

.titlebar {
  display: flex;
  align-items: baseline;
  gap: 10px;
  padding: 8px 12px;
  border-bottom: 1px solid var(--line);
  background: var(--panel);
}

.banner {
  padding: 10px 12px 0;
}

.spacer {
  flex: 1;
}

.name {
  display: flex;
  align-items: center;
  gap: 6px;
}

.name input {
  width: 220px;
}

.note {
  display: flex;
  align-items: center;
  gap: 10px;
  font-size: 13px;
}

.note span {
  flex: 1;
}

.saved {
  margin: 0;
  padding: 6px;
  overflow: auto;
  list-style: none;
}

.saved .row {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 2px;
  width: 100%;
  padding: 8px 10px;
  text-align: left;
}

.empty {
  padding: 16px 12px;
  font-size: 13px;
}

main {
  flex: 1;
  min-height: 0;
  display: grid;
  grid-template-columns: 260px 1fr 380px;
}

.overlay {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.6);
  display: grid;
  place-items: center;
}

.sheet {
  width: min(760px, 90vw);
  max-height: 80vh;
  display: flex;
  flex-direction: column;
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: 10px;
  overflow: hidden;
}

.sheet header {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 10px 12px;
  border-bottom: 1px solid var(--line);
}

.sheet h3 {
  margin: 0;
  flex: 1;
  font-size: 14px;
}

pre {
  margin: 0;
  padding: 12px;
  overflow: auto;
}
</style>
