<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, shallowRef } from "vue";
import TargetList from "./components/TargetList.vue";
import ElementList from "./components/ElementList.vue";
import StepComposer from "./components/StepComposer.vue";
import StepCard, { type Status } from "./components/StepCard.vue";
import Modal from "./components/Modal.vue";
import {
  api, asFailure, type ElementNode, type Failure, type Found, type Locator, type Observation,
  type Region, type Resource, type SavedRecording, type Step, type Targets,
} from "./api";
import { appMatches, inputsNeeded, move, nextId, toStep, type Draft } from "./steps";

const targets = shallowRef<Targets | null>(null);
const resource = shallowRef<Resource | null>(null);
const observation = shallowRef<Observation | null>(null);
const regions = ref<Region[]>([]);
const region = ref<string | null>(null);
const node = shallowRef<ElementNode | null>(null);
const picking = ref(false);
const destination = ref<Locator | null>(null);

const name = ref("新录制");
const steps = ref<Step[]>([]);
const statuses = ref<Record<string, { status: Status; error?: Failure | null }>>({});
const tried = ref<Record<string, Found | Failure | null>>({});

const loadingTargets = ref(false);
const loadingElements = ref(false);
const busy = ref(false);
const recording = ref(false);
const skipped = ref(0);
let poll: number | undefined;

const notice = ref<{ kind: "error" | "ok"; text: string; hint?: string } | null>(null);
const asking = ref<{ inputs: ReturnType<typeof inputsNeeded>; then: (values: Record<string, string>) => void } | null>(null);
const values = ref<Record<string, string>>({});
const browsing = ref<{ recordings: SavedRecording[]; directory: string } | null>(null);

function fail(error: unknown): void {
  const failure = asFailure(error);
  notice.value = { kind: "error", text: failure.message, hint: failure.hint };
}
function tell(text: string): void {
  notice.value = { kind: "ok", text };
  window.setTimeout(() => notice.value?.kind === "ok" && (notice.value = null), 3000);
}

async function refresh(): Promise<void> {
  loadingTargets.value = true;
  try {
    targets.value = await api.targets();
  } catch (error) {
    fail(error);
  } finally {
    loadingTargets.value = false;
  }
}

async function connect(port: number | null): Promise<void> {
  loadingTargets.value = true;
  try {
    targets.value = await api.connectBrowser(port);
    tell(port ? `已连接浏览器端口 ${port}` : "已断开浏览器");
  } catch (error) {
    fail(error);
  } finally {
    loadingTargets.value = false;
  }
}

async function select(next: Resource): Promise<void> {
  resource.value = next;
  region.value = null;
  regions.value = [];
  node.value = null;
  picking.value = false;
  await read();
  if (next.provider === "desktop.windows_uia") {
    api.observe(next.resource_id, { mode: "overview" })
      .then((overview) => (regions.value = overview.regions ?? []))
      .catch(() => (regions.value = []));
  }
}

async function read(): Promise<void> {
  if (!resource.value) return;
  loadingElements.value = true;
  try {
    observation.value = await api.observe(resource.value.resource_id, {
      mode: "outline", limit: 500, max_characters: 200000,
      ...(region.value ? { region: region.value } : {}),
    });
  } catch (error) {
    observation.value = null;
    fail(error);
  } finally {
    loadingElements.value = false;
  }
}

async function chooseRegion(next: string | null): Promise<void> {
  region.value = next;
  await read();
}

function pick(next: ElementNode): void {
  if (picking.value) {
    destination.value = next.step_locator;
    picking.value = false;
    return;
  }
  node.value = next;
  destination.value = null;
}

/** Ask for run-time inputs when the steps need any, then continue. */
function withInputs(needed: Step[], then: (values: Record<string, string>) => void): void {
  const inputs = inputsNeeded(needed);
  if (!inputs.length) return then({});
  values.value = Object.fromEntries(inputs.map((input) => [input.name, values.value[input.name] ?? ""]));
  asking.value = { inputs, then };
}

function confirmInputs(): void {
  const pending = asking.value;
  asking.value = null;
  pending?.then({ ...values.value });
}

async function performOne(step: Step, inputs: Record<string, string>): Promise<void> {
  statuses.value = { ...statuses.value, [step.id]: { status: "running" } };
  try {
    await api.perform(step, inputs);
    statuses.value = { ...statuses.value, [step.id]: { status: "succeeded" } };
  } catch (error) {
    statuses.value = { ...statuses.value, [step.id]: { status: "failed", error: asFailure(error) } };
  }
  await read();
}

async function add(draft: Draft, perform: boolean): Promise<void> {
  if (!node.value || !observation.value) return;
  const step = toStep(draft, observation.value.app, node.value.step_locator, steps.value);
  steps.value = [...steps.value, step];
  destination.value = null;
  if (perform) withInputs([step], (inputs) => performOne(step, inputs));
}

function update(index: number, step: Step): void {
  const next = [...steps.value];
  next[index] = step;
  steps.value = next;
}

function remove(index: number): void {
  steps.value = steps.value.filter((_, i) => i !== index);
}

async function test(step: Step, locator: Locator): Promise<void> {
  const window = targets.value?.resources.find((r) => appMatches(step.app, r)) ?? resource.value;
  if (!window) return fail({ code: "STEP.APP_NOT_FOUND", message: "找不到这个步骤的窗口" });
  try {
    tried.value = { ...tried.value, [step.id]: await api.tryLocator(window.resource_id, locator) };
  } catch (error) {
    tried.value = { ...tried.value, [step.id]: asFailure(error) };
  }
}

function runAll(): void {
  withInputs(steps.value, async (inputs) => {
    busy.value = true;
    statuses.value = Object.fromEntries(steps.value.map((s) => [s.id, { status: "running" as Status }]));
    try {
      const result = await api.run(name.value || "录制", steps.value, inputs);
      const next: typeof statuses.value = {};
      for (const step of steps.value) next[step.id] = { status: "skipped" };
      for (const outcome of result.steps) {
        next[outcome.id] = { status: outcome.status === "succeeded" ? "succeeded" : "failed", error: outcome.error };
      }
      statuses.value = next;
      if (result.status === "succeeded") tell(`全部 ${steps.value.length} 步已完成`);
      else if (result.error) fail(result.error);
    } catch (error) {
      statuses.value = {};
      fail(error);
    } finally {
      busy.value = false;
      await read();
    }
  });
}

function append(batch: { steps: Step[]; skipped: unknown[] }): void {
  const added: Step[] = [];
  for (const step of batch.steps) {
    const taken = [...steps.value, ...added];
    added.push(taken.some((s) => s.id === step.id) ? { ...step, id: nextId(taken, "rec") } : step);
  }
  steps.value = [...steps.value, ...added];
  skipped.value += batch.skipped.length;
}

async function startRecording(): Promise<void> {
  if (!resource.value) return;
  try {
    await api.startRecording(resource.value.resource_id);
    recording.value = true;
    skipped.value = 0;
    poll = window.setInterval(async () => {
      try {
        append(await api.collect());
      } catch (error) {
        fail(error);
        await stopRecording();
      }
    }, 800);
  } catch (error) {
    fail(error);
  }
}

async function stopRecording(): Promise<void> {
  window.clearInterval(poll);
  poll = undefined;
  if (!recording.value) return;
  recording.value = false;
  try {
    append(await api.stopRecording());
    tell(skipped.value ? `录制结束，${skipped.value} 个动作无法回放，已略过` : "录制结束");
  } catch (error) {
    fail(error);
  }
}

async function save(): Promise<void> {
  busy.value = true;
  try {
    const saved = await api.save(name.value.trim(), steps.value);
    tell(`已保存：${saved.recording_path}`);
  } catch (error) {
    fail(error);
  } finally {
    busy.value = false;
  }
}

async function browse(): Promise<void> {
  try {
    browsing.value = await api.saved();
  } catch (error) {
    fail(error);
  }
}

async function openSaved(path: string): Promise<void> {
  try {
    const opened = await api.open(path);
    name.value = opened.name;
    steps.value = opened.steps;
    statuses.value = {};
    browsing.value = null;
  } catch (error) {
    fail(error);
  }
}

function startOver(): void {
  steps.value = [];
  statuses.value = {};
  name.value = "新录制";
}

const canRecord = computed(
  () => !!targets.value?.recording_supported && resource.value?.provider === "desktop.windows_uia",
);
const recordTitle = computed(() =>
  !targets.value?.recording_supported
    ? "这个平台还不能自动录制，请用“添加步骤”"
    : canRecord.value ? "开始录制你在这个窗口里的操作" : "先在左侧选一个桌面窗口",
);

onMounted(refresh);
onBeforeUnmount(() => window.clearInterval(poll));
</script>

<template>
  <div class="shell">
    <header class="bar">
      <div class="brand">aad</div>
      <input v-model="name" class="name" placeholder="录制名称" />
      <span class="spacer" />
      <button v-if="!recording" class="record" :disabled="!canRecord" :title="recordTitle" @click="startRecording">
        <span class="dot" /> 录制
      </button>
      <button v-else class="record live" @click="stopRecording"><span class="dot" /> 停止录制</button>
      <button class="primary" :disabled="!steps.length || busy || recording" @click="runAll">▶ 运行全部</button>
      <button :disabled="!steps.length || busy || !name.trim()" @click="save">保存</button>
      <button @click="browse">打开…</button>
      <button class="ghost" :disabled="!steps.length" title="清空步骤" @click="startOver">新建</button>
    </header>

    <div v-if="notice" class="notice" :class="notice.kind">
      <span class="grow">{{ notice.text }}<span v-if="notice.hint" class="hint"> · {{ notice.hint }}</span></span>
      <button class="ghost" @click="notice = null">✕</button>
    </div>

    <main class="columns">
      <TargetList
        :targets="targets"
        :selected="resource?.resource_id ?? null"
        :loading="loadingTargets"
        @select="select"
        @refresh="refresh"
        @connect="connect"
      />
      <ElementList
        :observation="observation"
        :regions="regions"
        :region="region"
        :selected="node?.node_id ?? null"
        :loading="loadingElements"
        :picking="picking"
        :title="resource?.name ?? ''"
        @pick="pick"
        @region="chooseRegion"
        @reload="read"
      />
      <section class="panel">
        <header class="panel-head">
          步骤
          <span class="chip">{{ steps.length }}</span>
          <span v-if="recording" class="chip live-chip">● 录制中</span>
        </header>
        <StepComposer
          v-if="node"
          :node="node"
          :destination="destination"
          :busy="busy"
          @add="add"
          @pick-destination="picking = true"
          @close="node = null"
        />
        <div class="panel-body">
          <div v-if="!steps.length" class="empty">
            <p>还没有步骤。</p>
            <p class="muted">在中间选一个元素来添加步骤{{ targets?.recording_supported ? "，或点“录制”后直接操作窗口" : "" }}。</p>
          </div>
          <StepCard
            v-for="(step, index) in steps"
            :key="step.id"
            :step="step"
            :index="index"
            :last="index === steps.length - 1"
            :status="statuses[step.id]?.status"
            :error="statuses[step.id]?.error"
            :tried="tried[step.id]"
            @perform="withInputs([step], (inputs) => performOne(step, inputs))"
            @remove="remove(index)"
            @move="(by) => (steps = move(steps, index, by))"
            @update="(next) => update(index, next)"
            @test="(locator) => test(step, locator)"
          />
        </div>
      </section>
    </main>

    <Modal v-if="asking" title="运行时输入" @close="asking = null">
      <p class="muted">这些值只用于这次运行，不会保存。</p>
      <label v-for="input in asking.inputs" :key="input.name">
        {{ input.step }}
        <input v-model="values[input.name]" :type="input.sensitive ? 'password' : 'text'" />
      </label>
      <template #actions>
        <button class="ghost" @click="asking = null">取消</button>
        <button class="primary" @click="confirmInputs">继续</button>
      </template>
    </Modal>

    <Modal v-if="browsing" title="打开录制" @close="browsing = null">
      <p class="muted mono">{{ browsing.directory }}</p>
      <div v-if="!browsing.recordings.length" class="empty">还没有保存过录制</div>
      <button
        v-for="saved in browsing.recordings"
        :key="saved.path"
        class="saved"
        @click="openSaved(saved.path)"
      >
        <span class="grow ellipsis">{{ saved.name }}</span>
        <span class="muted">{{ saved.modified ?? "" }}</span>
      </button>
    </Modal>
  </div>
</template>

<style scoped>
.shell {
  height: 100%;
  display: flex;
  flex-direction: column;
}
.bar {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 10px 14px;
  background: var(--surface);
  border-bottom: 1px solid var(--line);
}
.brand {
  font-weight: 800;
  letter-spacing: 0.02em;
  padding: 2px 8px;
  border-radius: 6px;
  background: var(--accent);
  color: var(--on-accent);
}
.name {
  width: 240px;
  font-weight: 600;
  border-color: transparent;
  background: transparent;
}
.name:hover {
  border-color: var(--line);
}
.spacer {
  flex: 1;
}
.record .dot {
  display: inline-block;
  width: 8px;
  height: 8px;
  margin-right: 4px;
  border-radius: 50%;
  background: var(--record);
}
.record.live {
  border-color: var(--record);
  color: var(--record);
}
.record.live .dot {
  animation: pulse 1s infinite;
}
@keyframes pulse {
  50% {
    opacity: 0.25;
  }
}
.live-chip {
  color: var(--record);
}
.notice {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 14px;
  border-bottom: 1px solid var(--line);
}
.notice.error {
  background: var(--danger-soft);
  color: var(--danger);
}
.notice.ok {
  background: var(--ok-soft);
  color: var(--ok);
}
.hint {
  color: var(--text);
}
.columns {
  flex: 1;
  min-height: 0;
  display: grid;
  grid-template-columns: 260px minmax(320px, 1fr) minmax(360px, 440px);
  gap: 12px;
  padding: 12px;
}
.saved {
  display: flex;
  gap: 10px;
  width: 100%;
  text-align: left;
}
</style>
