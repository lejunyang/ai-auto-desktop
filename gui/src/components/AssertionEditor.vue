<script setup lang="ts">
import {
  ASSERTABLE_STATES,
  ASSERTION_MODES,
  type Assertion,
  type AssertionMode,
  type Step,
} from "../recording";

defineProps<{ step: Step }>();

// Only the patch: the enclosing list knows which step this belongs to and adds
// the id when it re-emits.
const emit = defineEmits<{
  change: [Partial<Assertion> | null];
}>();

/**
 * What each mode asks, in the words someone recording would use.
 *
 * The mode names are the saved format's, which is the wrong vocabulary for a
 * person deciding what to check.
 */
const MODE_LABELS: Record<AssertionMode, string> = {
  exists: "something appears",
  absent: "something disappears",
  value_equals: "the value is exactly",
  value_matches: "the value contains",
  state_equals: "the state becomes",
};

function update(patch: Partial<Assertion>): void {
  emit("change", patch);
}

function target(event: Event): string {
  return (event.target as HTMLInputElement | HTMLSelectElement).value;
}

/** Whether this mode compares against a typed value. */
function comparesValue(mode: AssertionMode): boolean {
  return mode === "value_equals" || mode === "value_matches";
}
</script>

<template>
  <div class="assertion">
    <label class="enable">
      <input
        type="checkbox"
        :checked="step.assertion !== undefined"
        @change="
          emit(
            'change',
            ($event.target as HTMLInputElement).checked
              ? ({ mode: 'exists' } as Partial<Assertion>)
              : null,
          )
        "
      />
      <span>check it worked</span>
    </label>

    <template v-if="step.assertion">
      <select
        class="mode"
        :value="step.assertion.mode"
        @change="update({ mode: target($event) as AssertionMode })"
      >
        <option v-for="mode in ASSERTION_MODES" :key="mode" :value="mode">
          {{ MODE_LABELS[mode] }}
        </option>
      </select>

      <input
        v-if="comparesValue(step.assertion.mode)"
        class="expected"
        :value="step.assertion.expected ?? ''"
        placeholder="expected text"
        @input="update({ expected: target($event) })"
      />

      <template v-if="step.assertion.mode === 'state_equals'">
        <select
          class="state"
          :value="step.assertion.state ?? 'enabled'"
          @change="update({ state: target($event) })"
        >
          <option v-for="state in ASSERTABLE_STATES" :key="state" :value="state">
            {{ state }}
          </option>
        </select>
        <select
          class="state"
          :value="step.assertion.expected ?? 'true'"
          @change="update({ expected: target($event) })"
        >
          <option value="true">true</option>
          <option value="false">false</option>
        </select>
      </template>

      <input
        class="timeout"
        :value="step.assertion.timeout ?? ''"
        placeholder="wait up to"
        title="How long to keep re-checking, such as 5s. Leave empty to check once."
        @input="update({ timeout: target($event) })"
      />
    </template>
  </div>
</template>

<style scoped>
.assertion {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 6px;
  margin-top: 6px;
}

.enable {
  display: flex;
  align-items: center;
  gap: 5px;
  color: var(--muted);
  font-size: 12px;
  white-space: nowrap;
}

.mode {
  flex: 0 1 auto;
}

.expected {
  flex: 1 1 120px;
  min-width: 90px;
}

/* Narrow: these hold short words like `enabled` and `true`, and letting them
   grow pushes the timeout field onto its own line. */
.state {
  flex: 0 0 auto;
}

.timeout {
  flex: 0 0 84px;
}
</style>
