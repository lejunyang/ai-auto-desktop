<script setup lang="ts">
import type { DriverFailure } from "../bridge";

defineProps<{ failure: DriverFailure }>();
defineEmits<{ dismiss: [] }>();
</script>

<template>
  <div class="failure" :class="{ retryable: failure.retryable }">
    <div class="head">
      <span class="code mono">{{ failure.code }}</span>
      <span v-if="failure.retryable" class="tag">retryable</span>
      <span v-if="failure.effect === 'unknown'" class="tag warn">effect unknown</span>
      <button class="close" @click="$emit('dismiss')">×</button>
    </div>
    <p class="message">{{ failure.message }}</p>
    <p v-if="failure.hint" class="hint">{{ failure.hint }}</p>
  </div>
</template>

<style scoped>
.failure {
  border: 1px solid var(--danger);
  border-radius: 8px;
  background: rgba(255, 107, 107, 0.08);
  padding: 8px 10px;
  margin-bottom: 10px;
}

.failure.retryable {
  border-color: var(--warn);
  background: rgba(255, 180, 84, 0.08);
}

.head {
  display: flex;
  align-items: center;
  gap: 8px;
}

.code {
  font-weight: 700;
}

.tag {
  font-size: 11px;
  padding: 1px 6px;
  border-radius: 10px;
  border: 1px solid var(--line);
  color: var(--muted);
}

.tag.warn {
  color: var(--warn);
  border-color: var(--warn);
}

.close {
  margin-left: auto;
  border: none;
  background: none;
  padding: 0 4px;
  color: var(--muted);
}

.message {
  margin: 6px 0 0;
}

.hint {
  margin: 4px 0 0;
  color: var(--muted);
  font-size: 13px;
}
</style>
