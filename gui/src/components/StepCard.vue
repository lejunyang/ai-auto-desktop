<script setup lang="ts">
import { ref } from "vue";
import type { Failure, Found, Locator, Step } from "../api";
import { cleanLocator, describeApp, title } from "../steps";

export type Status = "running" | "succeeded" | "failed" | "skipped";

const props = defineProps<{
  step: Step;
  index: number;
  last: boolean;
  status?: Status;
  error?: Failure | null;
  tried?: Found | Failure | null;
}>();
const emit = defineEmits<{
  perform: [];
  remove: [];
  move: [by: -1 | 1];
  update: [step: Step];
  test: [locator: Locator];
}>();

const open = ref(false);
const locator = ref<Locator>({});
const app = ref({ process: "", title: "", url: "" });
const contains = ref(false);
const within = ref("");
const nth = ref("");

function edit(): void {
  open.value = !open.value;
  if (!open.value) return;
  locator.value = { ...props.step.locator };
  contains.value = props.step.locator.match === "contains";
  within.value = props.step.locator.within?.name ?? "";
  nth.value = props.step.locator.nth?.toString() ?? "";
  app.value = {
    process: props.step.app.process ?? "",
    title: props.step.app.title ?? "",
    url: props.step.app.url ?? "",
  };
}

function edited(): Locator {
  const next: Locator = { ...locator.value, match: contains.value ? "contains" : undefined };
  next.within = within.value ? { name: within.value } : undefined;
  const position = nth.value.trim();
  next.nth = !position ? undefined : /^\d+$/.test(position) ? Number(position) : (position as "first" | "last");
  return cleanLocator(next);
}

function apply(): void {
  emit("update", {
    ...props.step,
    locator: edited(),
    app: cleanLocator({ ...props.step.app, ...app.value }),
  });
  open.value = false;
}

function triedText(): string {
  const tried = props.tried;
  if (!tried) return "";
  if ("code" in tried) return `${tried.message}${tried.hint ? " · " + tried.hint : ""}`;
  return tried.found ? "唯一匹配 ✓" : "现在找不到";
}
</script>

<template>
  <article class="card" :class="status">
    <div class="line">
      <span class="index">{{ index + 1 }}</span>
      <div class="grow">
        <div class="title">{{ title(step) }}</div>
        <div class="meta">
          <span class="chip">{{ describeApp(step.app) }}</span>
          <span v-if="step.input" class="chip accent">运行时输入</span>
          <span v-if="step.note" class="muted ellipsis note">{{ step.note }}</span>
        </div>
      </div>
      <span v-if="status === 'running'" class="state">…</span>
      <span v-else-if="status === 'succeeded'" class="state ok">✓</span>
      <span v-else-if="status === 'failed'" class="state bad">✕</span>
      <div class="tools">
        <button class="ghost" title="执行这一步" @click="emit('perform')">▶</button>
        <button class="ghost" title="编辑" @click="edit">✎</button>
        <button class="ghost" title="上移" :disabled="index === 0" @click="emit('move', -1)">↑</button>
        <button class="ghost" title="下移" :disabled="last" @click="emit('move', 1)">↓</button>
        <button class="ghost danger" title="删除" @click="emit('remove')">🗑</button>
      </div>
    </div>
    <div v-if="error && status === 'failed'" class="error">
      {{ error.message }}<span v-if="error.hint"> · {{ error.hint }}</span>
    </div>

    <div v-if="open" class="editor">
      <div class="grid">
        <label>角色 <input v-model="locator.role" /></label>
        <label>名称 <input v-model="locator.name" /></label>
        <label>自动化 ID <input v-model="locator.automation_id" placeholder="仅 Windows" /></label>
        <label>所在容器名称 <input v-model="within" placeholder="仅 Windows" /></label>
        <label>第几个 <input v-model="nth" placeholder="1、2…、first、last" /></label>
        <label class="inline"><input v-model="contains" type="checkbox" /> 名称按包含匹配</label>
      </div>
      <div class="grid">
        <label>进程或应用 <input v-model="app.process" /></label>
        <label>标题包含 <input v-model="app.title" /></label>
        <label>网址前缀 <input v-model="app.url" /></label>
      </div>
      <div class="foot">
        <button @click="emit('test', edited())">测试定位</button>
        <span class="muted grow ellipsis">{{ triedText() }}</span>
        <button class="ghost" @click="open = false">取消</button>
        <button class="primary" @click="apply">保存修改</button>
      </div>
    </div>
  </article>
</template>

<style scoped>
.card {
  border-bottom: 1px solid var(--line);
  padding: 10px 12px;
}
.card.failed {
  background: var(--danger-soft);
}
.card.succeeded .index {
  background: var(--ok-soft);
  color: var(--ok);
}
.line {
  display: flex;
  align-items: flex-start;
  gap: 10px;
}
.index {
  flex: 0 0 22px;
  height: 22px;
  border-radius: 50%;
  background: var(--surface-2);
  color: var(--muted);
  font-size: 11px;
  display: grid;
  place-items: center;
}
.title {
  font-weight: 500;
  word-break: break-word;
}
.meta {
  display: flex;
  gap: 6px;
  align-items: center;
  margin-top: 3px;
  min-width: 0;
}
.note {
  font-size: 11px;
}
.state {
  font-weight: 700;
}
.state.ok {
  color: var(--ok);
}
.state.bad {
  color: var(--danger);
}
.tools {
  display: flex;
  opacity: 0.35;
}
.card:hover .tools {
  opacity: 1;
}
.tools button {
  padding: 2px 6px;
}
.error {
  margin: 6px 0 0 32px;
  color: var(--danger);
  font-size: 12px;
}
.editor {
  margin: 10px 0 0 32px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.grid {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 8px;
  align-items: end;
}
.foot {
  display: flex;
  gap: 8px;
  align-items: center;
}
</style>
