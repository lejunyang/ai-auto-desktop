<script setup lang="ts">
defineProps<{ title: string }>();
const emit = defineEmits<{ close: [] }>();
</script>

<template>
  <div class="backdrop" @click.self="emit('close')" @keydown.esc="emit('close')">
    <div class="modal" role="dialog" :aria-label="title">
      <header class="panel-head">
        {{ title }}
        <span class="spacer" />
        <button class="ghost" title="关闭" @click="emit('close')">✕</button>
      </header>
      <div class="content"><slot /></div>
      <footer v-if="$slots.actions" class="actions"><slot name="actions" /></footer>
    </div>
  </div>
</template>

<style scoped>
.backdrop {
  position: fixed;
  inset: 0;
  background: rgb(10 12 16 / 45%);
  display: grid;
  place-items: center;
  z-index: 10;
}
.modal {
  width: min(520px, calc(100vw - 32px));
  max-height: calc(100vh - 64px);
  display: flex;
  flex-direction: column;
  background: var(--surface);
  border: 1px solid var(--line);
  border-radius: 12px;
  box-shadow: 0 12px 40px rgb(0 0 0 / 25%);
}
.content {
  padding: 14px;
  overflow: auto;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding: 10px 14px;
  border-top: 1px solid var(--line);
}
</style>
