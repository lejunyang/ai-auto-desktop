import { beforeEach, describe, expect, it } from "vitest";
import { Recording, resetIds, type StepDraft } from "./recording";
import { asFailure, BridgeError, bridge, setInvoker } from "./bridge";
import type { Element, WindowInfo } from "./bridge";

function element(overrides: Partial<Element> = {}): Element {
  return {
    node_id: "e9",
    ref: "snap123:1:e9",
    depth: 2,
    summary: 'role=Button name="Save"',
    actions: ["invoke", "pointer_click"],
    ...overrides,
  };
}

function windowInfo(overrides: Partial<WindowInfo> = {}): WindowInfo {
  return {
    window_id: "hwnd:100",
    title: "Editor",
    process_id: 42,
    process_name: "editor.exe",
    class_name: null,
    bounds: null,
    is_foreground: true,
    is_minimized: false,
    ...overrides,
  };
}

function draft(overrides: Partial<StepDraft> = {}): StepDraft {
  return { action: "invoke", element: element(), window: windowInfo(), ...overrides };
}

describe("Recording", () => {
  beforeEach(() => resetIds());

  it("captures the element reference so the step stays addressable", () => {
    const recording = new Recording();
    const step = recording.add(draft());

    expect(step.target).toBe("snap123:1:e9");
    expect(step.summary).toContain("Save");
    expect(step.enabled).toBe(true);
  });

  it("gives every step a distinct id", () => {
    const recording = new Recording();
    const first = recording.add(draft());
    const second = recording.add(draft());

    expect(first.id).not.toBe(second.id);
  });

  it("keeps a disabled step but leaves it out of the run", () => {
    const recording = new Recording();
    const step = recording.add(draft());
    recording.add(draft());

    recording.setEnabled(step.id, false);

    expect(recording.steps).toHaveLength(2);
    expect(recording.enabledSteps).toHaveLength(1);
    expect(recording.enabledSteps[0].id).not.toBe(step.id);
  });

  it("removes a step and reports whether anything was removed", () => {
    const recording = new Recording();
    const step = recording.add(draft());

    expect(recording.remove(step.id)).toBe(true);
    expect(recording.remove("missing")).toBe(false);
    expect(recording.steps).toHaveLength(0);
  });

  it("reorders steps", () => {
    const recording = new Recording();
    const first = recording.add(draft());
    const second = recording.add(draft());
    const third = recording.add(draft());

    recording.move(third.id, 0);

    expect(recording.steps.map((step) => step.id)).toEqual([
      third.id,
      first.id,
      second.id,
    ]);
  });

  it("refuses a move that is out of range instead of corrupting the order", () => {
    const recording = new Recording();
    const step = recording.add(draft());

    expect(recording.move(step.id, 5)).toBe(false);
    expect(recording.move("missing", 0)).toBe(false);
    expect(recording.steps).toHaveLength(1);
  });

  it("reports a text action with nothing to type", () => {
    const recording = new Recording();
    recording.add(draft({ action: "type_text" }));

    const issues = recording.validate();

    expect(issues).toHaveLength(1);
    expect(issues[0].message).toContain("text");
  });

  it("stops reporting once the missing text is supplied", () => {
    const recording = new Recording();
    const step = recording.add(draft({ action: "set_value" }));

    recording.setArgument(step.id, "hello");

    expect(recording.validate()).toHaveLength(0);
  });

  it("ignores problems in steps that are disabled", () => {
    const recording = new Recording();
    const step = recording.add(draft({ action: "type_text" }));
    recording.setEnabled(step.id, false);

    expect(recording.validate()).toHaveLength(0);
  });

  it("emits a descriptor the CLI would accept", () => {
    const recording = new Recording();
    recording.add(draft({ action: "invoke" }));

    const descriptor = recording.toDescriptor("demo") as Record<string, any>;

    expect(descriptor.apiVersion).toBe("ai-auto-desktop.dev/v1alpha1");
    expect(descriptor.kind).toBe("Workflow");
    expect(descriptor.metadata.name).toBe("demo");
    expect(descriptor.budgets.max_duration).toBeTruthy();
    expect(descriptor.steps[0].uses).toBe("desktop.windows_uia.invoke@1");
    expect(descriptor.steps[0].with.target).toBe("snap123:1:e9");
  });

  it("puts a text argument under the field each action expects", () => {
    const recording = new Recording();
    const typing = recording.add(draft({ action: "type_text" }));
    const setting = recording.add(draft({ action: "set_value" }));
    recording.setArgument(typing.id, "typed");
    recording.setArgument(setting.id, "assigned");

    const descriptor = recording.toDescriptor() as Record<string, any>;

    expect(descriptor.steps[0].with.text).toBe("typed");
    expect(descriptor.steps[1].with.value).toBe("assigned");
  });

  it("omits disabled steps from the descriptor", () => {
    const recording = new Recording();
    const skipped = recording.add(draft());
    recording.add(draft());
    recording.setEnabled(skipped.id, false);

    const descriptor = recording.toDescriptor() as Record<string, any>;

    expect(descriptor.steps).toHaveLength(1);
  });

  it("uses step ids the descriptor schema allows", () => {
    const recording = new Recording();
    recording.add(draft());

    const descriptor = recording.toDescriptor() as Record<string, any>;

    // Hyphens are not valid in a step id, so they must not survive.
    expect(descriptor.steps[0].id).not.toContain("-");
  });

  it("keeps the step budget above the number of steps", () => {
    const recording = new Recording();
    for (let index = 0; index < 12; index += 1) {
      recording.add(draft());
    }

    const descriptor = recording.toDescriptor() as Record<string, any>;

    expect(descriptor.budgets.max_executed_steps).toBeGreaterThanOrEqual(
      descriptor.steps.length,
    );
  });
});

describe("bridge", () => {
  it("passes arguments through to the backend command", async () => {
    const calls: Array<[string, unknown]> = [];
    setInvoker(async (command, args) => {
      calls.push([command, args]);
      return { windows: [], count: 0 };
    });

    await bridge.listApps();
    await bridge.describeWindow("hwnd:1", 50);

    expect(calls[0][0]).toBe("list_apps");
    expect(calls[1]).toEqual(["describe_window", { windowId: "hwnd:1", limit: 50 }]);
    setInvoker(null);
  });

  it("turns a structured backend failure into a usable error", async () => {
    setInvoker(async () => {
      throw {
        code: "DRIVER.STALE_HANDLE",
        message: "the UI moved on",
        retryable: false,
        effect: "not_applied",
        hint: "re-describe the window",
      };
    });

    await expect(bridge.listApps()).rejects.toThrowError(BridgeError);
    try {
      await bridge.listApps();
    } catch (error) {
      const failure = error as BridgeError;
      expect(failure.code).toBe("DRIVER.STALE_HANDLE");
      expect(failure.failure.hint).toContain("re-describe");
    }
    setInvoker(null);
  });

  it("never surfaces an unreadable error to the user", () => {
    // Whatever the backend throws, the UI must have something to show.
    expect(asFailure("plain text").message).toBe("plain text");
    expect(asFailure(new Error("boom")).message).toBe("boom");
    expect(asFailure({}).message).toBeTruthy();
    expect(asFailure(undefined).message).toBeTruthy();
    expect(asFailure(null).code).toBe("GUI.ERROR");
  });

  it("parses a failure that arrived as a JSON string", () => {
    const failure = asFailure('{"code":"DRIVER.NOT_FOUND","message":"no match","retryable":true}');

    expect(failure.code).toBe("DRIVER.NOT_FOUND");
    expect(failure.retryable).toBe(true);
  });
});
