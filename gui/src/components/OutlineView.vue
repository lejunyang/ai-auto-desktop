<script setup lang="ts">
import { computed, ref } from "vue";
import type { Element, Outline } from "../bridge";

const props = defineProps<{ outline: Outline | null; loading: boolean }>();
defineEmits<{ record: [Element, string]; refresh: [] }>();

const filter = ref("");

const visible = computed(() => {
  if (!props.outline) {
    return [];
  }
  const needle = filter.value.trim().toLowerCase();
  if (!needle) {
    return props.outline.elements;
  }
  return props.outline.elements.filter((element) =>
    element.summary.toLowerCase().includes(needle),
  );
});
</script>

<template>
  <section class="panel">
    <header>
      <h2>UI outline</h2>
      <input v-model="filter" placeholder="Filter elements" />
      <button :disabled="!outline || loading" @click="$emit('refresh')">
        {{ loading ? "…" : "Re-read" }}
      </button>
    </header>

    <p v-if="!outline" class="muted empty">
      Select a window to read its interface.
    </p>

    <template v-else>
      <div class="summary mono">
        {{ outline.node_count }} nodes · showing {{ visible.length }}
        <span v-if="outline.truncated" class="warn">· truncated</span>
        · snapshot {{ outline.snapshot_id }}@{{ outline.revision }}
      </div>

      <ul>
        <li v-for="element in visible" :key="element.node_id">
          <div class="row">
            <span
              class="summary-text"
              :style="{ paddingLeft: `${Math.min(element.depth, 8) * 10}px` }"
            >
              {{ element.summary }}
            </span>
            <span class="ref mono">{{ element.ref }}</span>
          </div>
          <div class="actions">
            <button
              v-for="action in element.actions"
              :key="action"
              @click="$emit('record', element, action)"
            >
              {{ action }}
            </button>
            <span v-if="!element.actions.length" class="muted mono">no actions</span>
          </div>
        </li>
      </ul>
    </template>
  </section>
</template>

<style scoped>
.panel {
  display: flex;
  flex-direction: column;
  min-height: 0;
  background: var(--bg);
}

header {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 10px 12px;
  border-bottom: 1px solid var(--line);
}

h2 {
  margin: 0;
  font-size: 13px;
  text-transform: uppercase;
  letter-spacing: 0.06em;
  color: var(--muted);
}

header input {
  flex: 1;
}

.summary {
  padding: 6px 12px;
  color: var(--muted);
  border-bottom: 1px solid var(--line);
}

.warn {
  color: var(--warn);
}

.empty {
  padding: 12px;
}

ul {
  list-style: none;
  margin: 0;
  padding: 0;
  overflow-y: auto;
  flex: 1;
}

li {
  padding: 6px 12px;
  border-bottom: 1px solid var(--line);
}

li:hover {
  background: var(--panel);
}

.row {
  display: flex;
  align-items: baseline;
  gap: 10px;
}

.summary-text {
  flex: 1;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.ref {
  color: var(--muted);
}

.actions {
  display: flex;
  gap: 6px;
  margin-top: 4px;
  flex-wrap: wrap;
}

.actions button {
  padding: 2px 8px;
  font-size: 12px;
}
</style>
