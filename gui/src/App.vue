<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, reactive, ref, shallowRef } from "vue";
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

// -------------------------------------------------------------------------
// Live recording
//
// Captured steps go straight into the same recording the step list is bound to,
// so they appear while the user is still working and can be corrected on the
// spot. A separate "review what was captured" screen would turn correcting a
// recording as it happens into recording first and sorting it out afterwards.
// -------------------------------------------------------------------------

interface CaptureState {
  captureId: string;
  window: WindowInfo;
  sources: string[];
  dropped: number;
  /** Steps adopted during this session, for the counter. */
  adopted: number;
  unresolved: number;
}

const capture = ref<CaptureState | null>(null);
const capturing = ref(false);
let poller: number | null = null;

/** Whether the session is missing a capture mechanism it would normally have. */
const partialCapture = computed(() => {
  const sources = capture.value?.sources ?? [];
  // Both mechanisms are needed for full coverage: WinForms controls are only
  // seen by the WinEvent hook, and Chromium's are only named by the UIA
  // handler. One source means a whole class of interaction goes unrecorded,
  // which is worth saying out loud rather than leaving to be discovered.
  return sources.length > 0 && sources.length < 2;
});

async function startCapture(): Promise<void> {
  if (!selected.value || capture.value) {
    return;
  }
  capturing.value = true;
  await guard(async () => {
    const session = await bridge.startRecording(selected.value!.window_id);
    capture.value = {
      captureId: session.capture_id,
      window: selected.value!,
      sources: session.sources,
      dropped: 0,
      adopted: 0,
      unresolved: 0,
    };
    // Poll rather than waiting for the end: the buffer is bounded, and the
    // point of recording in a window that is still open is seeing the steps
    // arrive.
    poller = window.setInterval(pollCapture, 700);
  });
  capturing.value = false;
}

async function pollCapture(): Promise<void> {
  const session = capture.value;
  if (!session) {
    return;
  }
  try {
    const batch = await bridge.collectRecording(session.captureId);
    session.dropped += batch.dropped;
    if (batch.steps.length) {
      const added = recording.addCaptured(
        batch.steps,
        session.window,
        windows.value,
      );
      session.adopted += added.length;
      session.unresolved += added.filter((step) => !step.enabled).length;
    }
  } catch (error) {
    // Stop polling on failure rather than reporting the same problem every
    // 700ms until someone notices.
    await stopCapture();
    failure.value = asFailure(error);
  }
}

async function stopCapture(): Promise<void> {
  const session = capture.value;
  if (poller !== null) {
    window.clearInterval(poller);
    poller = null;
  }
  if (!session) {
    return;
  }
  capture.value = null;
  await guard(async () => {
    // One last collect before releasing, or the interactions between the final
    // poll and the stop button would be lost -- which includes whatever the
    // user did immediately before deciding they were finished.
    const batch = await bridge.collectRecording(session.captureId);
    if (batch.steps.length) {
      recording.addCaptured(batch.steps, session.window, windows.value);
    }
    await bridge.stopRecording(session.captureId);
  });
}

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

// Leaving hooks installed after the window closes would outlive the app that
// asked for them.
onBeforeUnmount(() => {
  void stopCapture();
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
      <button
        v-if="!capture"
        @click="startCapture"
        :disabled="capturing || !selected"
        :title="selected ? 'Record what you do in ' + selected.title : 'Pick a window first'"
      >
        {{ capturing ? "Starting…" : "Record" }}
      </button>
      <button v-else class="recording" @click="stopCapture">
        ■ Stop ({{ capture.adopted }})
      </button>
      <button @click="save" :disabled="saving || !recording.steps.length">
        {{ saving ? "Saving…" : "Save" }}
      </button>
      <button @click="openList">Open…</button>
    </div>

    <div class="banner" v-if="failure">
      <FailureBanner :failure="failure" @dismiss="failure = null" />
    </div>

    <div class="banner live" v-if="capture">
      <span class="dot"></span>
      <span>
        Recording <strong>{{ capture.window.title }}</strong> — go and use it.
        Steps appear on the right as you work and can be corrected there.
      </span>
      <span class="muted mono">{{ capture.sources.join(" + ") || "no source" }}</span>
      <span class="warn" v-if="partialCapture">
        only one capture mechanism attached, so some interactions may go unrecorded
      </span>
      <span class="warn" v-if="capture.unresolved">
        {{ capture.unresolved }} step(s) need attention
      </span>
      <span class="warn" v-if="capture.dropped">
        {{ capture.dropped }} event(s) dropped
      </span>
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
        :window="selected"
        @toggle="(id, on) => recording.setEnabled(id, on)"
        @locator="(id, next) => recording.setLocator(id, next)"
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

.live {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 8px 12px;
  font-size: 13px;
  background: rgba(220, 60, 60, 0.12);
  border-bottom: 1px solid var(--line);
}

.live span:nth-child(2) {
  flex: 1;
}

.dot {
  width: 9px;
  height: 9px;
  border-radius: 50%;
  background: #e04b4b;
  animation: pulse 1.4s ease-in-out infinite;
}

@keyframes pulse {
  50% { opacity: 0.25; }
}

.warn {
  color: #e0a34b;
}

button.recording {
  border-color: #e04b4b;
  color: #e04b4b;
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
  /* minmax(0, 1fr) rather than 1fr: a fr track's minimum is its content width,
     so one long unbreakable string -- the snapshot id on every outline row --
     pushes the column past the viewport and carries the right-hand panel off
     screen with it. Measured before the fix: a 1536px viewport, a middle column
     grown to 1318px, and the Recording panel starting at x=1578. */
  grid-template-columns: 260px minmax(0, 1fr) 380px;
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
