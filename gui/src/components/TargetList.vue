<script setup lang="ts">
import { computed, ref } from "vue";
import type { Resource, Targets } from "../api";

const props = defineProps<{ targets: Targets | null; selected: string | null; loading: boolean }>();
const emit = defineEmits<{
  select: [resource: Resource];
  refresh: [];
  connect: [port: number | null];
}>();

const query = ref("");
const port = ref("");

const groups = computed(() => {
  const words = query.value.trim().toLowerCase();
  const shown = (props.targets?.resources ?? []).filter((resource) => {
    if (!words) return true;
    const process = String(resource.metadata.process_name ?? resource.metadata.name ?? "");
    return `${resource.name} ${process}`.toLowerCase().includes(words);
  });
  return [
    { title: "桌面窗口", items: shown.filter((r) => r.kind !== "browser") },
    { title: "浏览器页面", items: shown.filter((r) => r.kind === "browser") },
  ].filter((group) => group.items.length);
});

const unavailable = computed(() => Object.entries(props.targets?.unavailable ?? {}));

function processOf(resource: Resource): string {
  if (resource.kind === "browser") return String(resource.metadata.url ?? "");
  return String(resource.metadata.process_name ?? resource.metadata.name ?? resource.provider);
}

function connect(): void {
  const value = port.value.trim();
  emit("connect", value ? Number(value) : null);
}
</script>

<template>
  <section class="panel">
    <header class="panel-head">
      窗口
      <span class="spacer" />
      <button class="ghost" :disabled="loading" title="重新列出" @click="emit('refresh')">
        {{ loading ? "…" : "↻" }}
      </button>
    </header>
    <div class="search"><input v-model="query" placeholder="搜索窗口或进程" /></div>
    <div class="panel-body">
      <div v-if="!targets && loading" class="empty">正在列出窗口…</div>
      <div v-else-if="!groups.length" class="empty">没有可用的窗口</div>
      <div v-for="group in groups" :key="group.title" class="group">
        <div class="group-title">{{ group.title }}</div>
        <button
          v-for="resource in group.items"
          :key="resource.resource_id"
          class="target"
          :class="{ active: resource.resource_id === selected }"
          @click="emit('select', resource)"
        >
          <span class="name ellipsis">{{ resource.name || "（无标题）" }}</span>
          <span class="process ellipsis muted">{{ processOf(resource) }}</span>
        </button>
      </div>
      <div v-for="[name, reason] in unavailable" :key="name" class="unavailable">
        <strong>{{ name }}</strong> 不可用：{{ reason.message ?? reason.code }}
      </div>
    </div>
    <footer class="browser">
      <input v-model="port" class="grow" inputmode="numeric" placeholder="浏览器调试端口，如 9222" />
      <button @click="connect">{{ port ? "连接" : "断开" }}</button>
    </footer>
  </section>
</template>

<style scoped>
.search {
  padding: 8px 10px 4px;
}
.search input {
  width: 100%;
}
.group {
  padding: 4px 6px 8px;
}
.group-title {
  padding: 6px 8px 4px;
  font-size: 11px;
  color: var(--muted);
}
.target {
  display: flex;
  flex-direction: column;
  align-items: stretch;
  width: 100%;
  text-align: left;
  border: none;
  background: transparent;
  padding: 7px 9px;
  border-radius: 7px;
}
.target.active {
  background: var(--accent-soft);
}
.target.active .name {
  color: var(--accent);
  font-weight: 600;
}
.process {
  font-size: 11px;
}
.unavailable {
  margin: 6px 10px;
  padding: 8px;
  border-radius: 7px;
  background: var(--surface-2);
  color: var(--muted);
  font-size: 12px;
}
.browser {
  display: flex;
  gap: 6px;
  padding: 8px 10px;
  border-top: 1px solid var(--line);
}
</style>
