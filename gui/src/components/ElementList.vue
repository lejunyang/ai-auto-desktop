<script setup lang="ts">
import { computed, ref } from "vue";
import type { ElementNode, Observation, Region } from "../api";

const props = defineProps<{
  observation: Observation | null;
  regions: Region[];
  region: string | null;
  selected: string | null;
  loading: boolean;
  picking: boolean;
  title: string;
}>();
const emit = defineEmits<{
  pick: [node: ElementNode];
  region: [name: string | null];
  reload: [];
}>();

const query = ref("");
const actionableOnly = ref(true);

const nodes = computed(() => {
  const words = query.value.trim().toLowerCase();
  return (props.observation?.nodes ?? []).filter((node) => {
    if (actionableOnly.value && !node.capabilities.length) return false;
    if (!words) return true;
    return `${node.role} ${node.name} ${node.value ?? ""}`.toLowerCase().includes(words);
  });
});

const summary = computed(() => {
  const observed = props.observation;
  if (!observed) return "";
  const shown = observed.nodes?.length ?? 0;
  const matched = observed.matched ?? shown;
  return matched > shown ? `显示 ${shown} / ${matched}` : `${shown} 个元素`;
});
</script>

<template>
  <section class="panel">
    <header class="panel-head">
      <span class="ellipsis">{{ title || "元素" }}</span>
      <span class="chip">{{ summary }}</span>
      <span class="spacer" />
      <button class="ghost" :disabled="loading || !observation" title="重新读取" @click="emit('reload')">
        {{ loading ? "…" : "↻" }}
      </button>
    </header>
    <div v-if="picking" class="picking">点选拖放的目标元素</div>
    <div v-if="regions.length" class="regions">
      <button class="chip-button" :class="{ on: region === null }" @click="emit('region', null)">全部</button>
      <button
        v-for="item in regions"
        :key="item.name"
        class="chip-button"
        :class="{ on: region === item.name }"
        @click="emit('region', item.name)"
      >
        {{ item.name }}
      </button>
    </div>
    <div class="filters">
      <input v-model="query" class="grow" placeholder="按角色、名称或值筛选" />
      <label class="inline"><input v-model="actionableOnly" type="checkbox" /> 只看可操作</label>
    </div>
    <div class="panel-body">
      <div v-if="!observation && !loading" class="empty">从左侧选一个窗口</div>
      <div v-else-if="loading && !observation" class="empty">正在读取界面…</div>
      <div v-else-if="!nodes.length" class="empty">没有符合条件的元素</div>
      <button
        v-for="node in nodes"
        :key="node.node_id"
        class="node"
        :class="{ active: node.node_id === selected }"
        @click="emit('pick', node)"
      >
        <span class="role mono">{{ node.role }}</span>
        <span class="grow ellipsis">
          {{ node.name || "（无名称）" }}
          <span v-if="node.value" class="muted"> = {{ node.value }}</span>
        </span>
        <span v-if="node.capabilities.length" class="chip">{{ node.capabilities.length }} 项操作</span>
      </button>
    </div>
  </section>
</template>

<style scoped>
.picking {
  padding: 7px 12px;
  background: var(--accent-soft);
  color: var(--accent);
  font-size: 12px;
}
.regions {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  padding: 8px 12px 0;
}
.chip-button {
  padding: 2px 9px;
  border-radius: 999px;
  font-size: 12px;
}
.chip-button.on {
  background: var(--accent-soft);
  border-color: var(--accent);
  color: var(--accent);
}
.filters {
  display: flex;
  gap: 10px;
  align-items: center;
  padding: 8px 12px;
  border-bottom: 1px solid var(--line);
}
.node {
  display: flex;
  align-items: center;
  gap: 10px;
  width: 100%;
  text-align: left;
  border: none;
  border-bottom: 1px solid var(--line);
  border-radius: 0;
  background: transparent;
  padding: 7px 12px;
}
.node.active {
  background: var(--accent-soft);
}
.role {
  flex: 0 0 96px;
  color: var(--muted);
  overflow: hidden;
  text-overflow: ellipsis;
}
</style>
