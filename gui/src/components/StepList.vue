<script setup lang="ts">
import { computed } from "vue";
import { ACTIONS_NEEDING_TEXT, type Step, type ValidationIssue } from "../recording";

const props = defineProps<{ steps: Step[]; issues: ValidationIssue[] }>();

defineEmits<{
  toggle: [string, boolean];
  remove: [string];
  move: [string, number];
  argument: [string, string];
  export: [];
  clear: [];
}>();

const issuesByStep = computed(() => {
  const map = new Map<string, string[]>();
  for (const issue of props.issues) {
    const list = map.get(issue.stepId) ?? [];
    list.push(issue.message);
    map.set(issue.stepId, list);
  }
  return map;
});

function needsText(action: string): boolean {
  return (ACTIONS_NEEDING_TEXT as readonly string[]).includes(action);
}

/**
 * Show how the step will find its element at replay time.
 *
 * This is the durable description rather than a snapshot reference, so it is
 * what determines whether a reopened recording still works.
 */
function describeLocator(step: Step): string {
  if (!step.locator) {
    return "cannot be identified — narrow the window or pick another element";
  }
  if (!step.window) {
    return "the window cannot be told apart from another one that was open";
  }
  const parts = Object.entries(step.locator)
    .filter(([, value]) => value !== undefined && value !== null && value !== "")
    .map(([field, value]) => `${field}=${JSON.stringify(value)}`);
  return parts.join(" ");
}
</script>

<template>
  <section class="panel">
    <header>
      <h2>Recording</h2>
      <span class="muted mono">{{ steps.length }} steps</span>
      <button :disabled="!steps.length" @click="$emit('clear')">Clear</button>
      <button
        class="primary"
        :disabled="!steps.length || issues.length > 0"
        @click="$emit('export')"
      >
        Export
      </button>
    </header>

    <p v-if="!steps.length" class="muted empty">
      Pick an action in the outline to record a step.
    </p>

    <ol>
      <li
        v-for="(step, index) in steps"
        :key="step.id"
        :class="{ disabled: !step.enabled, invalid: issuesByStep.has(step.id) }"
      >
        <div class="row">
          <input
            type="checkbox"
            :checked="step.enabled"
            @change="
              $emit('toggle', step.id, ($event.target as HTMLInputElement).checked)
            "
          />
          <span class="action mono">{{ step.action }}</span>
          <span class="target">{{ step.summary }}</span>
          <span class="spacer" />
          <button :disabled="index === 0" @click="$emit('move', step.id, index - 1)">
            ↑
          </button>
          <button
            :disabled="index === steps.length - 1"
            @click="$emit('move', step.id, index + 1)"
          >
            ↓
          </button>
          <button @click="$emit('remove', step.id)">×</button>
        </div>

        <div class="row second">
          <span class="window mono">{{ step.windowTitle }}</span>
          <span class="ref mono">{{ describeLocator(step) }}</span>
        </div>

        <input
          v-if="needsText(step.action)"
          class="argument"
          :value="step.argument ?? ''"
          placeholder="text to enter"
          @input="
            $emit('argument', step.id, ($event.target as HTMLInputElement).value)
          "
        />

        <p
          v-for="message in issuesByStep.get(step.id) ?? []"
          :key="message"
          class="issue"
        >
          {{ message }}
        </p>
      </li>
    </ol>
  </section>
</template>

<style scoped>
.panel {
  display: flex;
  flex-direction: column;
  min-height: 0;
  background: var(--panel);
  border-left: 1px solid var(--line);
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

ol {
  list-style: none;
  margin: 0;
  padding: 0;
  overflow-y: auto;
  flex: 1;
  counter-reset: step;
}

li {
  padding: 8px 12px;
  border-bottom: 1px solid var(--line);
}

li.disabled {
  opacity: 0.5;
}

li.invalid {
  box-shadow: inset 3px 0 0 var(--warn);
}

.row {
  display: flex;
  align-items: center;
  gap: 8px;
}

.row.second {
  margin-top: 3px;
}

.action {
  color: var(--accent);
  font-weight: 600;
}

.target {
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  max-width: 40%;
}

.spacer {
  flex: 1;
}

.row button {
  padding: 1px 7px;
}

.window,
.ref {
  color: var(--muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.argument {
  margin-top: 6px;
  width: 100%;
}

.issue {
  margin: 5px 0 0;
  color: var(--warn);
  font-size: 12px;
}
</style>
