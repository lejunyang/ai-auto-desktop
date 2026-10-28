/**
 * Pure step logic: what to call an action, how a form becomes a step, and
 * how a step reads in the list. Nothing here talks to the backend.
 */
import type { Action, ActionType, App, ElementNode, Locator, Step } from "./api";

/** Everything a person can add for an element, in the order offered. */
export type Choice = ActionType | "wait_present" | "wait_absent";

export const LABELS: Record<Choice, string> = {
  invoke: "触发",
  click: "点击",
  fill: "填写",
  type_text: "输入文字",
  key: "按键",
  focus: "聚焦",
  select: "选择选项",
  check: "勾选",
  scroll: "滚动",
  hover: "悬停",
  drag: "拖到…",
  navigate: "打开网址",
  back: "后退",
  forward: "前进",
  reload: "刷新",
  wait_present: "等它出现",
  wait_absent: "等它消失",
};

const ORDER: ActionType[] = [
  "invoke", "click", "fill", "type_text", "key", "select", "check",
  "scroll", "hover", "drag", "focus", "navigate", "back", "forward", "reload",
];

/** The actions an element offers, plus the two waits every element allows. */
export function choicesFor(node: ElementNode): Choice[] {
  const offered = ORDER.filter((name) => node.capabilities.includes(name));
  return [...offered, "wait_present", "wait_absent"];
}

/** The form behind the composer: one field per parameter any action takes. */
export interface Draft {
  choice: Choice;
  button: "left" | "right" | "middle";
  double: boolean;
  text: string;
  /** Take the text from a run-time input instead of saving it. */
  asInput: boolean;
  mode: "unicode" | "keys";
  keys: string;
  direction: "up" | "down" | "left" | "right";
  notches: number;
  ms: number;
  option: string;
  checked: boolean;
  url: string;
  withinMs: number;
  to: Locator | null;
}

export function blankDraft(choice: Choice): Draft {
  return {
    choice, button: "left", double: false, text: "", asInput: false, mode: "unicode",
    keys: "", direction: "down", notches: 3, ms: 500, option: "", checked: true,
    url: "https://", withinMs: 5000, to: null,
  };
}

/** Why a draft cannot become a step yet, or null when it can. */
export function draftProblem(draft: Draft): string | null {
  switch (draft.choice) {
    case "key":
      return parseKeys(draft.keys).length ? null : "写下按键，例如 ctrl,a";
    case "select":
      return draft.option ? null : "选一个选项";
    case "navigate":
      return /^https?:\/\/.+/.test(draft.url) ? null : "网址要以 http:// 或 https:// 开头";
    case "drag":
      return draft.to ? null : "在元素列表里点选拖放目标";
    case "fill":
    case "type_text":
      return draft.asInput || draft.text || draft.choice === "fill" ? null : "写下要输入的文字";
    default:
      return null;
  }
}

export function parseKeys(raw: string): string[] {
  return raw.split(",").map((key) => key.trim()).filter(Boolean);
}

function action(draft: Draft): Action | undefined {
  switch (draft.choice) {
    case "click": {
      const click: Action = { type: "click" };
      if (draft.button !== "left") click.button = draft.button;
      if (draft.double) click.count = 2;
      return click;
    }
    case "fill":
      return draft.asInput ? { type: "fill" } : { type: "fill", value: draft.text };
    case "type_text":
      return draft.asInput
        ? { type: "type_text", mode: draft.mode }
        : { type: "type_text", text: draft.text, mode: draft.mode };
    case "key":
      return { type: "key", keys: parseKeys(draft.keys) };
    case "scroll":
      return { type: "scroll", direction: draft.direction, notches: draft.notches };
    case "hover":
      return { type: "hover", ms: draft.ms };
    case "drag":
      return draft.button === "left" ? { type: "drag" } : { type: "drag", button: draft.button };
    case "select":
      return { type: "select", option: draft.option };
    case "check":
      return { type: "check", checked: draft.checked };
    case "navigate":
      return { type: "navigate", url: draft.url };
    case "wait_present":
    case "wait_absent":
      return undefined;
    default:
      return { type: draft.choice };
  }
}

/** The next unused id, step1, step2 and so on. */
export function nextId(steps: Step[], prefix = "step"): string {
  const taken = new Set(steps.map((step) => step.id));
  let n = steps.length + 1;
  while (taken.has(`${prefix}${n}`)) n += 1;
  return `${prefix}${n}`;
}

export function toStep(draft: Draft, app: App, locator: Locator, steps: Step[]): Step {
  const id = nextId(steps);
  const step: Step = { id, app: { ...app }, locator: { ...locator } };
  if (draft.choice === "wait_present" || draft.choice === "wait_absent") {
    step.check = { present: draft.choice === "wait_present", within_ms: draft.withinMs };
    return step;
  }
  step.action = action(draft);
  if (draft.choice === "drag" && draft.to) step.to = { ...draft.to };
  if (draft.asInput && (draft.choice === "fill" || draft.choice === "type_text")) {
    step.input = { name: `${id}_text`, sensitive: true };
  }
  return step;
}

/** How an element reads: its role, then its name in corner quotes. */
export function describeLocator(locator: Locator): string {
  const parts: string[] = [];
  if (locator.role) parts.push(locator.role);
  if (locator.name) parts.push(`「${locator.name}」`);
  if (locator.automation_id) parts.push(`#${locator.automation_id}`);
  if (locator.within?.name) parts.push(`在「${locator.within.name}」里`);
  if (locator.near?.anchor?.name) parts.push(`挨着「${locator.near.anchor.name}」`);
  if (locator.nth !== undefined) parts.push(`第 ${locator.nth} 个`);
  return parts.join(" ") || "（空定位）";
}

export function describeApp(app: App): string {
  const parts = [app.process, app.title && `标题含「${app.title}」`, app.url];
  return parts.filter(Boolean).join(" · ") || app.provider || "任意窗口";
}

const BUTTONS = { left: "左键", right: "右键", middle: "中键" } as const;
const DIRECTIONS = { up: "向上", down: "向下", left: "向左", right: "向右" } as const;

/** One line saying what a step does. */
export function title(step: Step): string {
  const target = describeLocator(step.locator);
  if (step.check) return `${step.check.present ? "等待出现" : "等待消失"} ${target}`;
  const action = step.action;
  if (!action) return target;
  const secret = step.input ? "（运行时输入）" : "";
  switch (action.type) {
    case "click":
      return `${action.count === 2 ? "双击" : action.button && action.button !== "left" ? BUTTONS[action.button] + "点击" : "点击"} ${target}`;
    case "fill":
      return `在 ${target} 填写${secret || `「${action.value ?? ""}」`}`;
    case "type_text":
      return `在 ${target} 输入${secret || `「${action.text ?? ""}」`}`;
    case "key":
      return `在 ${target} 按 ${action.keys.join("+")}`;
    case "scroll":
      return `在 ${target} ${DIRECTIONS[action.direction]}滚动 ${action.notches ?? 3} 格`;
    case "hover":
      return `悬停在 ${target}`;
    case "drag":
      return `把 ${target} 拖到 ${step.to ? describeLocator(step.to) : "？"}`;
    case "select":
      return `在 ${target} 选择「${action.option}」`;
    case "check":
      return `${action.checked ? "勾选" : "取消勾选"} ${target}`;
    case "navigate":
      return `打开 ${action.url}`;
    default:
      return `${LABELS[action.type]} ${target}`;
  }
}

/** Whether a listed window is the one a step names; mirrors the backend. */
export function appMatches(app: App, resource: { provider: string; name: string; metadata: Record<string, unknown> }): boolean {
  const text = (value: unknown) => (typeof value === "string" ? value.toLowerCase() : null);
  const names = ["process_name", "name", "bundle_id", "application_name"]
    .map((key) => text(resource.metadata[key]))
    .filter((value): value is string => value !== null);
  return (
    (!app.provider || resource.provider === app.provider) &&
    (!app.process || names.includes(app.process.toLowerCase())) &&
    (!app.title || (text(resource.name) ?? "").includes(app.title.toLowerCase())) &&
    (!app.url || String(resource.metadata.url ?? "").startsWith(app.url))
  );
}

/** The run-time inputs a list of steps needs, in step order. */
export function inputsNeeded(steps: Step[]): { name: string; sensitive: boolean; step: string }[] {
  return steps.filter((step) => step.input).map((step) => ({ ...step.input!, step: title(step) }));
}

export function move<T>(items: T[], index: number, by: -1 | 1): T[] {
  const target = index + by;
  if (target < 0 || target >= items.length) return items;
  const next = [...items];
  [next[index], next[target]] = [next[target], next[index]];
  return next;
}

/** A locator or app with empty fields dropped, as the backend expects. */
export function cleanLocator<T extends object>(value: T): T {
  const clean: Record<string, unknown> = {};
  for (const [key, field] of Object.entries(value)) {
    if (field === "" || field === undefined || field === null) continue;
    clean[key] = field;
  }
  return clean as T;
}
