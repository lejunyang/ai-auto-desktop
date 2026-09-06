<script setup lang="ts">
import { computed, ref } from "vue";
import type { Element, Outline, Survey } from "../bridge";

const props = defineProps<{
  outline: Outline | null;
  survey: Survey | null;
  region: string | null;
  loading: boolean;
}>();
defineEmits<{
  record: [Element, string];
  refresh: [];
  region: [string | null];
}>();

const filter = ref("");

/**
 * Whether the region list is expanded while a region is open.
 *
 * Measured: left expanded it takes 252px of a 771px panel and the element list
 * shows 7 of 18 rows. Once a region is open its only remaining use is switching to
 * another one, and the breadcrumb already covers going back, so it folds to a line
 * and can be reopened.
 */
const showingRegions = ref(false);

/**
 * Whether to put the regions in front of the list.
 *
 * A whole-window listing stops on the character budget in 9 of 23 windows here,
 * the worst showing 80 of 271 elements, and nothing the user does to the limit
 * changes that. When it happens the regions are the only complete account of what
 * the window holds, so they lead. On a window that fits -- 14 of 23 -- they would
 * be a step in the way, so they stay out of it.
 */
const leadWithRegions = computed(
  () => Boolean(props.outline?.truncated) && !props.region,
);

/** The region list is open when it leads, or when asked for inside a region. */
const regionsVisible = computed(
  () => Boolean(props.survey) && (leadWithRegions.value || (Boolean(props.region) && showingRegions.value)),
);

/** Elements the truncated listing never reached, if the regions can say. */
const unreached = computed(() => {
  if (!props.survey || !props.outline) {
    return 0;
  }
  const named = props.survey.regions.reduce((sum, region) => sum + region.elements, 0);
  return Math.max(0, named - props.outline.elements.length);
});

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
        <span v-if="outline.truncated" class="warn">
          · {{ unreached ? `${unreached} more in the regions below` : "truncated" }}
        </span>
        · snapshot {{ outline.snapshot_id }}@{{ outline.revision }}
      </div>

      <div class="crumb mono" v-if="region">
        <button class="link" @click="$emit('region', null)">← all regions</button>
        <span>{{ region }}</span>
        <button class="link" @click="showingRegions = !showingRegions">
          {{ showingRegions ? "hide regions" : "switch region" }}
        </button>
      </div>

      <p class="muted lead" v-if="leadWithRegions">
        {{
          outline.stopped_by === "characters"
            ? "This window holds more than one listing can carry, and a longer list will not help. Pick a region to see what is in it."
            : "The listing was cut short. Pick a region to see one part in full."
        }}
      </p>

      <ul class="regions" v-if="regionsVisible && survey">
        <li
          v-for="group in survey.regions"
          :key="group.region"
          :class="{ current: group.region === region }"
        >
          <button class="region-row" @click="$emit('region', group.region)">
            <span class="region-name">{{ group.region }}</span>
            <span class="muted mono">{{ group.elements }}</span>
          </button>
          <span class="holds muted mono">
            {{ group.holds.map((h) => `${h.count} ${h.role}`).join(", ") }}
          </span>
        </li>
        <li class="muted mono" v-if="survey.folded_regions">
          {{ survey.folded_regions }} single-element region(s) folded away
        </li>
      </ul>



      <ul v-if="!leadWithRegions">
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
  /* A grid item defaults to min-width: auto, so long content in the
     outline would widen the column instead of scrolling inside it. */
  min-width: 0;
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

.crumb {
  display: flex;
  align-items: center;
  gap: 0.5rem;
  padding: 0.3rem 0.6rem;
  font-size: 0.75rem;
}

.link {
  background: none;
  border: none;
  color: #7cc4ff;
  cursor: pointer;
  padding: 0;
  font: inherit;
}

.regions {
  list-style: none;
  margin: 0;
  padding: 0 0.4rem;
  /* Takes the space that is there rather than a fixed ceiling. The previous
     18rem was set when a window offered at most twelve regions; splitting the
     unattributed one by role takes that to 24, and the fixed height then showed
     6 of them with 456px of the panel sitting empty below. `min-height: 0` is
     what lets a flex child shrink at all -- without it the minimum is the
     content height and the list never scrolls. */
  flex: 1;
  min-height: 0;
  overflow-y: auto;
}

.lead {
  padding: 0.3rem 0.6rem 0;
  margin: 0;
  /* Above the list, not below it: the list now fills the panel, so anything
     after it is pushed out of sight. */
}

.regions li {
  padding: 0.2rem 0;
  border-bottom: 1px solid #1d2530;
}

.regions li.current .region-name {
  color: #7cc4ff;
}

.region-row {
  display: flex;
  width: 100%;
  justify-content: space-between;
  gap: 0.6rem;
  background: none;
  border: none;
  color: inherit;
  cursor: pointer;
  font: inherit;
  text-align: left;
  padding: 0.15rem 0;
}

.region-row:hover .region-name {
  color: #7cc4ff;
}

.holds {
  display: block;
  font-size: 0.68rem;
  padding-left: 0.2rem;
}
</style>
