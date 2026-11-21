<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from "vue";
import type { ElementNode, Locator, Resource, Step } from "../api";
import { api, asFailure } from "../api";
import { appMatches, describeApp, describeLocator } from "../steps";

const props = defineProps<{ step: Step; shots: File[]; node: ElementNode | null; resource: Resource | null; busy: boolean }>();
const emit = defineEmits<{ replace: [step: Step]; close: [] }>();
const shotIndex = ref(0);
const original = ref("");
const current = ref("");
const message = ref("");
const checked = ref(false);
const checking = ref(false);
let revision = 0;
const proposed = computed(() => props.node?.step_locator);

function releaseImage() {
  if (original.value) URL.revokeObjectURL(original.value);
  original.value = "";
}
watch([() => props.shots, shotIndex], () => {
  releaseImage();
  const file = props.shots[shotIndex.value];
  if (file) original.value = URL.createObjectURL(new Blob([file], { type: "image/png" }));
}, { immediate: true });
watch([() => props.node, () => props.resource], () => {
  revision++;
  current.value = "";
  message.value = "";
  checked.value = false;
  checking.value = false;
});
onBeforeUnmount(() => { revision++; releaseImage(); });

async function inspect(locator: Locator) {
  const resource = props.resource;
  if (!resource) return;
  const request = ++revision;
  checked.value = false;
  current.value = "";
  checking.value = true;
  try {
    if (props.step.action && !props.node?.capabilities.includes(props.step.action.type)) {
      throw new Error("当前元素未提供这一步需要的操作");
    }
    const found = await api.tryLocator(resource.resource_id, locator);
    if (request !== revision) return;
    if (!found.found || found.match_count !== 1) throw new Error("该定位未匹配到唯一元素");
    checked.value = true;
    message.value = "当前窗口唯一匹配";
    try {
      const shot = await api.capture(resource.resource_id, locator);
      if (request === revision && shot.data) current.value = "data:image/png;base64," + shot.data;
    } catch {
      if (request === revision) message.value += "，当前元素截图不可用";
    }
  } catch (error) {
    if (request === revision) message.value = asFailure(error).message;
  } finally {
    if (request === revision) checking.value = false;
  }
}

function replace() {
  if (checked.value && proposed.value && props.resource && !props.busy) {
    const app = appMatches(props.step.app, props.resource) ? props.step.app : props.resource.app;
    emit("replace", { ...props.step, locator: proposed.value, app });
  }
}
</script>

<template>
  <section class="repair">
    <header><strong>修复 {{ step.id }}</strong><button class="ghost" @click="emit('close')">关闭</button></header>
    <div class="compare">
      <div>
        <b>录制时</b><p>{{ describeApp(step.app) }} · {{ describeLocator(step.locator ?? {}) }}</p>
        <img v-if="original" :src="original" alt="此录制步骤的页面截图" />
        <small v-else>此步骤没有录制截图</small>
        <select v-if="shots.length > 1" v-model="shotIndex" aria-label="录制截图">
          <option v-for="(_, i) in shots" :key="i" :value="i">截图 {{ i + 1 }}</option>
        </select>
      </div>
      <div>
        <b>现在</b><p>{{ proposed && resource ? resource.name + ' · ' + describeLocator(proposed) : '在窗口和元素列表中选择新目标' }}</p>
        <img v-if="current" :src="current" alt="当前选中元素的截图" />
        <small role="status">{{ message }}</small>
      </div>
    </div>
    <footer>
      <button :disabled="!proposed || checking || busy" @click="proposed && inspect(proposed)">检查所选元素</button>
      <button class="primary" :disabled="!checked || checking || busy" @click="replace">替换步骤目标</button>
    </footer>
  </section>
</template>

<style scoped>
.repair { background: var(--surface-2); padding: 12px; }
header, footer { display: flex; justify-content: space-between; align-items: center; gap: 8px; }
.compare { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; margin: 10px 0; }
.compare > div { min-width: 0; }
p { overflow-wrap: anywhere; margin: 4px 0; }
img { display: block; max-width: 100%; max-height: 180px; object-fit: contain; }
footer { justify-content: end; }
</style>
