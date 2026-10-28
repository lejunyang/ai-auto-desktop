<script setup lang="ts">
import { computed, ref, watch } from "vue";
import type { ElementNode, Locator } from "../api";
import { blankDraft, choicesFor, describeLocator, draftProblem, LABELS, type Choice, type Draft } from "../steps";

const props = defineProps<{ node: ElementNode; destination: Locator | null; busy: boolean }>();
const emit = defineEmits<{
  add: [draft: Draft, perform: boolean];
  pickDestination: [];
  close: [];
}>();

const choices = computed(() => choicesFor(props.node));
const draft = ref<Draft>(blankDraft(choices.value[0]));

watch(
  () => props.node.node_id,
  () => (draft.value = blankDraft(choices.value[0])),
);
watch(
  () => props.destination,
  (to) => (draft.value.to = to),
);

function choose(choice: Choice): void {
  draft.value = { ...blankDraft(choice), text: draft.value.text };
  if (choice === "select") draft.value.option = props.node.options?.[0] ?? "";
  if (choice === "drag") emit("pickDestination");
}

const problem = computed(() => draftProblem(draft.value));
</script>

<template>
  <div class="composer">
    <div class="head">
      <div class="grow">
        <div class="muted small">为这个元素添加步骤</div>
        <div class="target ellipsis">{{ describeLocator(node.step_locator) }}</div>
      </div>
      <button class="ghost" title="关闭" @click="emit('close')">✕</button>
    </div>

    <div class="choices">
      <button
        v-for="choice in choices"
        :key="choice"
        :class="{ on: draft.choice === choice }"
        @click="choose(choice)"
      >
        {{ LABELS[choice] }}
      </button>
    </div>

    <div class="fields">
      <template v-if="draft.choice === 'click'">
        <label>按键
          <select v-model="draft.button">
            <option value="left">左键</option><option value="right">右键</option><option value="middle">中键</option>
          </select>
        </label>
        <label class="inline"><input v-model="draft.double" type="checkbox" /> 双击</label>
      </template>

      <template v-if="draft.choice === 'fill' || draft.choice === 'type_text'">
        <label class="wide">文字
          <input v-model="draft.text" :disabled="draft.asInput" :placeholder="draft.asInput ? '运行时再输入' : ''" />
        </label>
        <label class="inline"><input v-model="draft.asInput" type="checkbox" /> 运行时输入，不保存（密码等）</label>
        <label v-if="draft.choice === 'type_text'">方式
          <select v-model="draft.mode">
            <option value="unicode">Unicode</option><option value="keys">按键盘布局逐键</option>
          </select>
        </label>
      </template>

      <label v-if="draft.choice === 'key'" class="wide">按键（逗号分隔，修饰键在前）
        <input v-model="draft.keys" placeholder="ctrl,a　或　enter　或　cmd,s" />
      </label>

      <template v-if="draft.choice === 'scroll'">
        <label>方向
          <select v-model="draft.direction">
            <option value="down">向下</option><option value="up">向上</option>
            <option value="left">向左</option><option value="right">向右</option>
          </select>
        </label>
        <label>格数 <input v-model.number="draft.notches" type="number" min="1" max="30" /></label>
      </template>

      <label v-if="draft.choice === 'hover'">停留毫秒 <input v-model.number="draft.ms" type="number" min="1" max="5000" /></label>

      <template v-if="draft.choice === 'drag'">
        <div class="wide destination">
          <span class="muted">目标：</span>
          <span class="grow ellipsis">{{ draft.to ? describeLocator(draft.to) : "还没选" }}</span>
          <button @click="emit('pickDestination')">在列表里点选</button>
        </div>
      </template>

      <label v-if="draft.choice === 'select'" class="wide">选项
        <select v-if="node.options?.length" v-model="draft.option">
          <option v-for="option in node.options" :key="option" :value="option">{{ option }}</option>
        </select>
        <input v-else v-model="draft.option" placeholder="选项文字或值" />
      </label>

      <label v-if="draft.choice === 'check'">状态
        <select v-model="draft.checked">
          <option :value="true">勾选</option><option :value="false">取消勾选</option>
        </select>
      </label>

      <label v-if="draft.choice === 'navigate'" class="wide">网址 <input v-model="draft.url" /></label>

      <label v-if="draft.choice === 'wait_present' || draft.choice === 'wait_absent'">最多等待毫秒
        <input v-model.number="draft.withinMs" type="number" min="100" max="30000" step="500" />
      </label>
    </div>

    <div class="foot">
      <span class="muted small grow">{{ problem ?? "" }}</span>
      <button :disabled="!!problem || busy" @click="emit('add', draft, false)">添加</button>
      <button class="primary" :disabled="!!problem || busy" @click="emit('add', draft, true)">添加并执行</button>
    </div>
  </div>
</template>

<style scoped>
.composer {
  border-bottom: 1px solid var(--line);
  background: var(--surface-2);
  padding: 12px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.head {
  display: flex;
  align-items: flex-start;
  gap: 8px;
}
.small {
  font-size: 11px;
}
.target {
  font-weight: 600;
}
.choices {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}
.choices button {
  padding: 3px 10px;
  border-radius: 999px;
}
.choices button.on {
  background: var(--accent);
  border-color: var(--accent);
  color: var(--on-accent);
}
.fields {
  display: flex;
  flex-wrap: wrap;
  gap: 10px;
  align-items: flex-end;
}
.fields:empty {
  display: none;
}
.wide {
  flex: 1 1 100%;
}
.destination {
  display: flex;
  align-items: center;
  gap: 6px;
}
.foot {
  display: flex;
  align-items: center;
  gap: 8px;
}
</style>
