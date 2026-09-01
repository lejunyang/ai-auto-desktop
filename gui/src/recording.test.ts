import { beforeEach, describe, expect, it } from "vitest";
import {
  ACTIONS_NEEDING_TEXT,
  RECORDING_API_VERSION,
  RECORDING_KIND,
  Recording,
  resetIds,
  type StepDraft,
} from "./recording";
import { asFailure, BridgeError, bridge, setInvoker } from "./bridge";
import type { Element, WindowInfo } from "./bridge";

function element(overrides: Partial<Element> = {}): Element {
  return {
    node_id: "e9",
    ref: "snap123:1:e9",
    locator: { role: "Button", name: "Save" },
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
    class_name: "EditorClass",
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

  it("stores a locator, not a snapshot reference", () => {
    // A `snapshot:revision:node` reference stops resolving once its snapshot is
    // gone, which was confirmed against a live desktop: replaying one after the
    // store was cleared fails with DRIVER.STALE_HANDLE. So a recording that is
    // meant to be saved has to describe the element instead of pointing at it.
    const recording = new Recording();

    const step = recording.add(draft());

    expect(step.locator).toEqual({ role: "Button", name: "Save" });
    expect(JSON.stringify(step)).not.toContain("snap123");
  });

  it("gives every step a distinct id", () => {
    const recording = new Recording();

    const first = recording.add(draft());
    const second = recording.add(draft());

    expect(first.id).not.toBe(second.id);
  });

  it("records an unidentifiable element but leaves it out of the run", () => {
    // The driver refuses an ambiguous locator, so a step that cannot be
    // narrowed would fail at replay. Keeping it visible but disabled says what
    // needs fixing; dropping it would lose the user's action silently.
    const recording = new Recording();

    const step = recording.add(draft({ element: element({ locator: null }) }));

    expect(recording.steps).toHaveLength(1);
    expect(step.enabled).toBe(false);
    expect(recording.enabledSteps).toHaveLength(0);
  });

  it("refuses to enable a step that cannot be located", () => {
    const recording = new Recording();
    const step = recording.add(draft({ element: element({ locator: null }) }));

    expect(recording.setEnabled(step.id, true)).toBe(false);
    expect(step.enabled).toBe(false);
  });

  it("keeps a disabled step but leaves it out of the run", () => {
    const recording = new Recording();
    const step = recording.add(draft());
    recording.add(draft());

    recording.setEnabled(step.id, false);

    expect(recording.steps).toHaveLength(2);
    expect(recording.enabledSteps).toHaveLength(1);
  });

  it("reports a text action with nothing to type", () => {
    const recording = new Recording();
    const step = recording.add(draft({ action: "set_value" }));

    const issues = recording.validate();

    expect(issues).toHaveLength(1);
    expect(issues[0].stepId).toBe(step.id);
    expect(recording.canExport).toBe(false);
  });

  it("accepts a text action once it has text", () => {
    const recording = new Recording();
    const step = recording.add(draft({ action: "type_text" }));

    recording.setArgument(step.id, "hello");

    expect(recording.validate()).toHaveLength(0);
    expect(recording.canExport).toBe(true);
  });

  it("does not demand text for actions that take none", () => {
    const recording = new Recording();
    recording.add(draft({ action: "invoke" }));

    expect(recording.validate()).toHaveLength(0);
    expect(ACTIONS_NEEDING_TEXT).not.toContain("invoke");
  });

  it("refuses to export a recording with nothing enabled", () => {
    const recording = new Recording();
    const step = recording.add(draft());
    recording.setEnabled(step.id, false);

    expect(recording.canExport).toBe(false);
    expect(recording.validate().some((issue) => issue.blocking)).toBe(true);
  });

  it("moves a step and keeps the rest in order", () => {
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

  it("ignores a move that goes nowhere valid", () => {
    const recording = new Recording();
    recording.add(draft());

    expect(recording.move("step_404", 0)).toBe(false);
    expect(recording.move("step_1", 5)).toBe(false);
  });

  it("removes a step", () => {
    const recording = new Recording();
    const step = recording.add(draft());

    expect(recording.remove(step.id)).toBe(true);
    expect(recording.steps).toHaveLength(0);
    expect(recording.remove(step.id)).toBe(false);
  });
});

describe("compiling to a workflow", () => {
  beforeEach(() => resetIds());

  it("expands each recorded action into snapshot, find and act", () => {
    // An action needs a reference valid in the current session, but only a
    // locator survives being saved. The gap is closed at replay time: read the
    // window now, find the element now, then act on what was just found.
    const recording = new Recording();
    recording.add(draft());

    const steps = recording.toDescriptor().steps as Record<string, unknown>[];

    expect(steps.map((step) => step.uses)).toEqual([
      "desktop.windows_uia.snapshot@1",
      "desktop.windows_uia.find@1",
      "desktop.windows_uia.invoke@1",
    ]);
  });

  it("passes the found reference to the action rather than a saved one", () => {
    const recording = new Recording();
    const step = recording.add(draft());

    const steps = recording.toDescriptor().steps as Record<string, unknown>[];
    const action = steps[2].with as Record<string, string>;

    expect(action.target).toBe(`\${{ steps.${step.id}_element.output.ref }}`);
  });

  it("searches the snapshot it just captured", () => {
    const recording = new Recording();
    const step = recording.add(draft());

    const steps = recording.toDescriptor().steps as Record<string, unknown>[];
    const find = steps[1].with as Record<string, unknown>;

    expect(find.snapshot_id).toBe(
      `\${{ steps.${step.id}_window.output.snapshot_id }}`,
    );
    expect(find.locator).toEqual({ role: "Button", name: "Save" });
  });

  it("identifies the window by class rather than by its title", () => {
    // A title changes as soon as the document is edited or renamed; a window
    // class does not.
    const recording = new Recording();
    recording.add(draft());

    const steps = recording.toDescriptor().steps as Record<string, unknown>[];
    const snapshot = steps[0].with as Record<string, unknown>;

    expect(snapshot.window).toEqual({ class_name: "EditorClass" });
  });

  it("adds a title only when two windows are otherwise identical", () => {
    // Found by replaying against a real desktop: two Notepad windows share a
    // class and a process, and the driver refuses an ambiguous selector rather
    // than picking one, so class alone would not have replayed.
    const target = windowInfo({ window_id: "hwnd:1", title: "a.txt - Notepad" });
    const rival = windowInfo({ window_id: "hwnd:2", title: "b.txt - Notepad" });
    const recording = new Recording();

    recording.add(draft({ window: target, openWindows: [target, rival] }));

    const steps = recording.toDescriptor().steps as Record<string, unknown>[];
    expect((steps[0].with as Record<string, unknown>).window).toEqual({
      class_name: "EditorClass",
      process_name: "editor.exe",
      title: "a.txt - Notepad",
    });
  });

  it("distinguishes by process when the class is shared", () => {
    const target = windowInfo({ window_id: "hwnd:1", class_name: "Shared" });
    const rival = windowInfo({
      window_id: "hwnd:2",
      class_name: "Shared",
      process_name: "other.exe",
      title: "Other",
    });
    const recording = new Recording();

    recording.add(draft({ window: target, openWindows: [target, rival] }));

    const steps = recording.toDescriptor().steps as Record<string, unknown>[];
    expect((steps[0].with as Record<string, unknown>).window).toEqual({
      class_name: "Shared",
      process_name: "editor.exe",
    });
  });

  it("refuses to record a window it cannot tell apart at all", () => {
    // Identical class, process and title. Enabling this would produce a
    // workflow that fails with DRIVER.AMBIGUOUS_MATCH at replay.
    const target = windowInfo({ window_id: "hwnd:1" });
    const twin = windowInfo({ window_id: "hwnd:2" });
    const recording = new Recording();

    const step = recording.add(draft({ window: target, openWindows: [target, twin] }));

    expect(step.window).toBeNull();
    expect(step.enabled).toBe(false);
    expect(recording.validate()[0].message).toMatch(/window cannot be told apart/);
  });

  it("falls back to a title when nothing sturdier is known", () => {
    const recording = new Recording();
    recording.add(
      draft({
        window: windowInfo({ class_name: null, process_name: null, title: "Untitled" }),
      }),
    );

    const steps = recording.toDescriptor().steps as Record<string, unknown>[];

    expect((steps[0].with as Record<string, unknown>).window).toEqual({
      title: "Untitled",
    });
  });

  it("budgets for the expansion, not the recorded count", () => {
    // Three executed steps per recorded action. A budget set from the recorded
    // count would abort a valid recording partway through.
    const recording = new Recording();
    recording.add(draft());
    recording.add(draft());
    recording.add(draft());

    const descriptor = recording.toDescriptor();
    const steps = descriptor.steps as unknown[];
    const budgets = descriptor.budgets as Record<string, number>;

    expect(steps).toHaveLength(9);
    expect(budgets.max_executed_steps).toBeGreaterThanOrEqual(9);
  });

  it("leaves disabled steps out of the compiled workflow", () => {
    const recording = new Recording();
    const step = recording.add(draft());
    recording.add(draft());
    recording.setEnabled(step.id, false);

    const steps = recording.toDescriptor().steps as unknown[];

    expect(steps).toHaveLength(3);
  });

  it("carries the text of a set_value action", () => {
    const recording = new Recording();
    const step = recording.add(draft({ action: "set_value" }));
    recording.setArgument(step.id, "typed by the recording");

    const steps = recording.toDescriptor().steps as Record<string, unknown>[];

    expect((steps[2].with as Record<string, string>).value).toBe(
      "typed by the recording",
    );
  });
});

describe("saving and reopening", () => {
  beforeEach(() => resetIds());

  it("round-trips a recording without losing anything", () => {
    const recording = new Recording();
    recording.name = "my-recording";
    const first = recording.add(draft({ action: "set_value" }));
    recording.setArgument(first.id, "text");
    recording.add(draft());

    const reopened = Recording.fromDocument(recording.toDocument());

    expect(reopened.name).toBe("my-recording");
    expect(reopened.steps).toEqual(recording.steps);
  });

  it("produces the same workflow after a round trip", () => {
    // What actually matters: reopening and replaying must run what was saved.
    const recording = new Recording();
    recording.add(draft());

    const before = recording.toDescriptor("fixed");
    const after = Recording.fromDocument(recording.toDocument()).toDescriptor("fixed");

    expect(after).toEqual(before);
  });

  it("saves a document that declares what it is", () => {
    const recording = new Recording();
    recording.add(draft());

    const document = recording.toDocument();

    expect(document.apiVersion).toBe(RECORDING_API_VERSION);
    expect(document.kind).toBe(RECORDING_KIND);
  });

  it("keeps the editable source distinct from the compiled workflow", () => {
    // Compilation is one-way: a workflow can express far more than a recording,
    // so rebuilding one from a workflow would lose the parts a person edits.
    const recording = new Recording();
    recording.add(draft());

    expect(recording.toDocument().kind).toBe("Recording");
    expect(recording.toDescriptor().kind).toBe("Workflow");
  });

  it("keeps disabled steps across a round trip", () => {
    const recording = new Recording();
    const step = recording.add(draft());
    recording.add(draft());
    recording.setEnabled(step.id, false);

    const reopened = Recording.fromDocument(recording.toDocument());

    expect(reopened.steps).toHaveLength(2);
    expect(reopened.steps[0].enabled).toBe(false);
  });

  it("refuses a file from an unknown format version", () => {
    // Loading a newer format by ignoring what it does not recognise would
    // replay fewer steps than the file describes.
    const document = { apiVersion: "ai-auto-desktop.dev/v2", kind: "Recording", steps: [] };

    expect(() => Recording.fromDocument(document)).toThrow(/apiVersion/);
  });

  it("refuses a file that is not a recording", () => {
    const workflow = new Recording().toDescriptor();

    expect(() => Recording.fromDocument(workflow)).toThrow(/not a recording/);
  });

  it("refuses a file with duplicate step ids", () => {
    // Two steps with one id would collide in the compiled workflow.
    const document = {
      apiVersion: RECORDING_API_VERSION,
      kind: RECORDING_KIND,
      metadata: { name: "dupes" },
      steps: [
        { id: "step_1", action: "invoke", locator: { role: "Button" }, enabled: true },
        { id: "step_1", action: "invoke", locator: { role: "Edit" }, enabled: true },
      ],
    };

    expect(() => Recording.fromDocument(document)).toThrow(/more than once/);
  });

  it("refuses a step with no action", () => {
    const document = {
      apiVersion: RECORDING_API_VERSION,
      kind: RECORDING_KIND,
      steps: [{ id: "step_1", locator: { role: "Button" } }],
    };

    expect(() => Recording.fromDocument(document)).toThrow(/action/);
  });

  it("will not enable a step that has no locator, whatever the file claims", () => {
    // A hand-edited file could assert this; enabling it would produce a
    // workflow that fails at replay.
    const document = {
      apiVersion: RECORDING_API_VERSION,
      kind: RECORDING_KIND,
      steps: [{ id: "step_1", action: "invoke", locator: null, enabled: true }],
    };

    const reopened = Recording.fromDocument(document);

    expect(reopened.steps[0].enabled).toBe(false);
  });

  it("will not enable a step whose window cannot be found, whatever the file claims", () => {
    const document = {
      apiVersion: RECORDING_API_VERSION,
      kind: RECORDING_KIND,
      steps: [
        {
          id: "step_1",
          action: "invoke",
          locator: { role: "Button" },
          window: null,
          enabled: true,
        },
      ],
    };

    expect(Recording.fromDocument(document).steps[0].enabled).toBe(false);
  });

  it("rejects something that is not an object at all", () => {
    for (const rubbish of [null, "text", 42, []]) {
      expect(() => Recording.fromDocument(rubbish)).toThrow();
    }
  });

  it("does not reuse an id already present in the loaded file", () => {
    // A generated id colliding with a loaded one would silently merge steps.
    const recording = new Recording();
    recording.add(draft());
    recording.add(draft());
    const reopened = Recording.fromDocument(recording.toDocument());

    const added = reopened.add(draft());

    expect(reopened.steps.filter((step) => step.id === added.id)).toHaveLength(1);
  });
});

describe("the bridge", () => {
  it("passes the reference through untouched", async () => {
    const seen: unknown[] = [];
    setInvoker(async (command, args) => {
      seen.push({ command, args });
      return { applied: true, action: "invoke", node_id: "e9" };
    });

    await bridge.act("invoke", "snap:2:e9");

    expect(seen[0]).toEqual({
      command: "act",
      args: { action: "invoke", target: "snap:2:e9", argument: null },
    });
  });

  it("sends both documents when saving so they cannot disagree", async () => {
    const seen: Record<string, unknown>[] = [];
    setInvoker(async (command, args) => {
      seen.push({ command, args });
      return { recording_path: "a", workflow_path: "b" };
    });

    await bridge.saveRecording("demo", { kind: "Recording" }, { kind: "Workflow" });

    expect(seen[0].command).toBe("save_recording");
    const args = seen[0].args as Record<string, unknown>;
    expect(args.name).toBe("demo");
    expect((args.document as Record<string, string>).kind).toBe("Recording");
    expect((args.workflow as Record<string, string>).kind).toBe("Workflow");
  });

  it("turns a structured failure into something displayable", () => {
    const failure = asFailure(
      new BridgeError({
        code: "STORE.NAME_INVALID",
        message: "a name may not contain '/'",
        retryable: false,
        effect: "not_applied",
        hint: "Choose a simpler name.",
      }),
    );

    expect(failure.code).toBe("STORE.NAME_INVALID");
    expect(failure.hint).toBe("Choose a simpler name.");
  });

  it("does not lose an error that arrives as a bare string", () => {
    const failure = asFailure("the backend panicked");

    expect(failure.message).toContain("panicked");
    expect(failure.code).toBeTruthy();
  });

  it("does not lose an error that arrives as an Error", () => {
    const failure = asFailure(new Error("no window"));

    expect(failure.message).toContain("no window");
  });
});
