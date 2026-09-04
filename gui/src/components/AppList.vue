<script setup lang="ts">
import type { WindowInfo } from "../bridge";

defineProps<{
  windows: WindowInfo[];
  selected: string | null;
  loading: boolean;
}>();

defineEmits<{ select: [WindowInfo]; refresh: [] }>();
</script>

<template>
  <section class="panel">
    <header>
      <h2>Running apps</h2>
      <button :disabled="loading" @click="$emit('refresh')">
        {{ loading ? "…" : "Refresh" }}
      </button>
    </header>

    <p v-if="!windows.length && !loading" class="muted empty">
      No windows found yet.
    </p>

    <ul>
      <li
        v-for="window in windows"
        :key="window.window_id"
        :class="{ active: window.window_id === selected }"
        @click="$emit('select', window)"
      >
        <div class="title">{{ window.title || "(untitled)" }}</div>
        <div class="meta mono">
          {{ window.process_name ?? "?" }} · pid {{ window.process_id }}
          <span v-if="window.is_foreground" class="fg">foreground</span>
        </div>
      </li>
    </ul>
  </section>
</template>

<style scoped>
.panel {
  /* A grid item defaults to min-width: auto, so long content in the
     outline would widen the column instead of scrolling inside it. */
  min-width: 0;
  display: flex;
  flex-direction: column;
  min-height: 0;
  background: var(--panel);
  border-right: 1px solid var(--line);
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
  flex: 1;
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
  padding: 8px 12px;
  border-bottom: 1px solid var(--line);
  cursor: pointer;
}

li:hover {
  background: var(--panel-2);
}

li.active {
  background: var(--panel-2);
  box-shadow: inset 3px 0 0 var(--accent);
}

.title {
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.meta {
  color: var(--muted);
  margin-top: 2px;
}

.fg {
  color: var(--ok);
  margin-left: 6px;
}
</style>
