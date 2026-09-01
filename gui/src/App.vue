<script setup lang="ts">
import { computed, onMounted, reactive, ref, shallowRef } from "vue";
import AppList from "./components/AppList.vue";
import OutlineView from "./components/OutlineView.vue";
import StepList from "./components/StepList.vue";
import FailureBanner from "./components/FailureBanner.vue";
import { asFailure, bridge, type DriverFailure, type Element, type Outline, type WindowInfo }
  from "./bridge";
import { Recording } from "./recording";

const windows = ref<WindowInfo[]>([]);
const selected = ref<WindowInfo | null>(null);
const outline = shallowRef<Outline | null>(null);
const failure = ref<DriverFailure | null>(null);
const loadingApps = ref(false);
const loadingOutline = ref(false);
const exported = ref<string | null>(null);

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
  recording.add({ action, element, window: selected.value });
}

function exportDescriptor(): void {
  exported.value = JSON.stringify(recording.toDescriptor(), null, 2);
}

async function copyExport(): Promise<void> {
  if (exported.value) {
    await navigator.clipboard.writeText(exported.value);
  }
}

onMounted(refreshApps);
</script>

<template>
  <div class="shell">
    <div class="titlebar">
      <strong>ai-auto-desktop</strong>
      <span class="muted">observe, then act</span>
    </div>

    <div class="banner" v-if="failure">
      <FailureBanner :failure="failure" @dismiss="failure = null" />
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
