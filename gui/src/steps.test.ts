import { describe, expect, it } from "vitest";
import type { ElementNode, Step } from "./api";
import {
  blankDraft, choicesFor, cleanLocator, draftProblem, inputsNeeded, move, nextId, title, toStep,
} from "./steps";

const app = { provider: "desktop.windows_uia", process: "notepad.exe" };
const save = { role: "Button", name: "保存" };
const node = (capabilities: string[]): ElementNode => ({
  node_id: "n1", role: "Button", name: "保存", capabilities, step_locator: save,
});

describe("choices", () => {
  it("offers only what the element supports, plus the waits", () => {
    expect(choicesFor(node(["click", "invoke"]))).toEqual([
      "invoke", "click", "wait_present", "wait_absent",
    ]);
  });
});

describe("drafts become steps", () => {
  it("keeps only the parameters that differ from the defaults", () => {
    const draft = { ...blankDraft("click"), button: "right" as const };
    const step = toStep(draft, app, save, []);
    expect(step).toEqual({ id: "step1", app, locator: save, action: { type: "click", button: "right" } });
    expect(title(step)).toBe("右键点击 Button 「保存」");
  });

  it("names a recorded canvas node and its distinguishing connection", () => {
    const board = { name: "结算图" };
    expect(draftProblem(blankDraft("click_object"))).toContain("标签");
    const step = toStep({ ...blankDraft("click_object"), objectLabel: "审批" }, app, board, []);
    expect(step.action).toEqual({ type: "click_object", query: { label: "审批" } });
    step.action = { type: "click_object", query: { label: "审批", after: "付款", view_hint: { fx: .4, fy: .5 } } };
    expect(title(step)).toBe("在 「结算图」 点击「审批」（「付款」 → 「审批」）");
    step.action = { type: "click_object", query: { id: "approved" }, options: { source: "editor" } };
    expect(title(step)).toBe("在 「结算图」 点击「approved」");
  });

  it("turns secret text into a run-time input that is never saved", () => {
    const draft = { ...blankDraft("fill"), text: "hunter2", asInput: true };
    const step = toStep(draft, app, { role: "Edit", name: "密码" }, []);
    expect(step.action).toEqual({ type: "fill" });
    expect(step.input).toEqual({ name: "step1_text", sensitive: true });
    expect(JSON.stringify(step)).not.toContain("hunter2");
    expect(inputsNeeded([step])[0].name).toBe("step1_text");
  });

  it("makes waits into checks rather than actions", () => {
    const step = toStep({ ...blankDraft("wait_absent"), withinMs: 2000 }, app, save, []);
    expect(step.action).toBeUndefined();
    expect(step.check).toEqual({ present: false, within_ms: 2000 });
    expect(title(step)).toBe("等待消失 Button 「保存」");
  });

  it("says what is missing before a step can be added", () => {
    expect(draftProblem({ ...blankDraft("key"), keys: " , " })).toMatch(/按键/);
    expect(draftProblem({ ...blankDraft("navigate"), url: "file:///etc" })).toMatch(/http/);
    expect(draftProblem(blankDraft("drag"))).toMatch(/目标/);
    expect(draftProblem({ ...blankDraft("key"), keys: "ctrl, a" })).toBeNull();
    const drag = toStep({ ...blankDraft("drag"), to: { role: "List", name: "回收站" } }, app, save, []);
    expect(drag.to).toEqual({ role: "List", name: "回收站" });
    expect(title(drag)).toBe("把 Button 「保存」 拖到 List 「回收站」");
  });
});

describe("lists", () => {
  it("names a receipt check without a step-level locator", () => {
    expect(title({id:"receipt", app, read:[{as:"amount", locator:save, property:"value"}], expect:{amount:"20"}})).toBe("读取并核对 amount");
  });
  it("numbers new steps past any already taken", () => {
    const steps = [{ id: "step2" }, { id: "rec1" }] as Step[];
    expect(nextId(steps)).toBe("step3");
  });

  it("moves within bounds and leaves the ends alone", () => {
    expect(move([1, 2, 3], 0, 1)).toEqual([2, 1, 3]);
    expect(move([1, 2, 3], 0, -1)).toEqual([1, 2, 3]);
  });

  it("drops empty locator fields", () => {
    expect(cleanLocator({ role: "Button", name: "", automation_id: undefined })).toEqual({ role: "Button" });
  });
});
