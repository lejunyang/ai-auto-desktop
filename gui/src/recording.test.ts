import { beforeEach, describe, expect, it } from "vitest";
import {
  ACTIONS_NEEDING_TEXT,
  RECORDING_API_VERSION,
  RECORDING_KIND,
  Recording,
  resetIds,
  selectorFor,
  type StepDraft,
  type CapturedStep,
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

  it("writes ordinary typed text into the file as it was recorded", () => {
    // Measured against a real desktop: an ordinary edit reports its value
    // through UIA, and that text is usually the point of the recording. Hiding
    // it would make a saved workflow unreadable for no gain in safety.
    const recording = new Recording();
    recording.add(draft({ action: "type_text", argument: "quarterly-report-2026" }));

    const descriptor = recording.toDescriptor();
    const steps = descriptor.steps as Record<string, unknown>[];

    expect((steps[2].with as Record<string, unknown>).text).toBe("quarterly-report-2026");
    // Nothing was externalised, so the descriptor keeps its previous shape.
    expect(descriptor.inputs).toBeUndefined();
  });

  it("keeps a password out of the file by turning it into an input", () => {
    // A recording is a file people copy, commit and share, so a credential
    // typed into a protected field must not be baked into it.
    const recording = new Recording();
    const step = recording.add(
      draft({
        action: "type_text",
        argument: "hunter2-real-secret",
        element: element({ protected: true }),
      }),
    );

    const descriptor = recording.toDescriptor();
    const steps = descriptor.steps as Record<string, unknown>[];
    const inputs = descriptor.inputs as Record<string, Record<string, unknown>>;

    expect((steps[2].with as Record<string, unknown>).text).toBe(
      `\${{ inputs.${step.id}_secret }}`,
    );
    expect(inputs[`${step.id}_secret`]).toEqual({
      schema: { type: "string" },
      required: true,
      sensitive: true,
    });
    // The decisive check: the secret is nowhere in the saved file.
    expect(JSON.stringify(descriptor)).not.toContain("hunter2-real-secret");
  });

  it("keeps a password out of the recording file too", () => {
    // The compiled workflow and the recording sit in the same directory. Taking
    // the secret out of one and writing it verbatim into the other would leave
    // it on disk while looking like it had been handled.
    const recording = new Recording();
    recording.add(
      draft({
        action: "type_text",
        argument: "hunter2-real-secret",
        element: element({ protected: true }),
      }),
    );

    const document = recording.toDocument();

    expect(JSON.stringify(document)).not.toContain("hunter2-real-secret");
    const steps = document.steps as Record<string, unknown>[];
    expect(steps[0].argument).toBeUndefined();
    expect(steps[0].protected).toBe(true);
  });

  it("does not ask for text a protected step is not supposed to store", () => {
    // Its value arrives at run time as an input, so an empty argument is the
    // expected state rather than an unfinished step.
    const recording = new Recording();
    recording.add(draft({ action: "type_text", element: element({ protected: true }) }));

    expect(recording.validate()).toEqual([]);
    expect(recording.canExport).toBe(true);
  });

  it("gives two recorded passwords separate inputs", () => {    // One shared input would silently type the same credential into both
    // fields, which is wrong for a sign-in that has a password and a
    // confirmation, or for two different accounts.
    const recording = new Recording();
    const first = recording.add(
      draft({ action: "type_text", argument: "first", element: element({ protected: true }) }),
    );
    const second = recording.add(
      draft({ action: "set_value", argument: "second", element: element({ protected: true }) }),
    );

    const descriptor = recording.toDescriptor();
    const inputs = descriptor.inputs as Record<string, unknown>;

    expect(Object.keys(inputs).sort()).toEqual(
      [`${first.id}_secret`, `${second.id}_secret`].sort(),
    );
  });

  it("searches the snapshot it just captured", () => {    const recording = new Recording();
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

  it("skips a class name the toolkit regenerates on every run", () => {
    // Measured by restarting a WinForms fixture: the same window reported
    // ...0.34473a7_r14_ad1 and then ...0.376a1c9_r8_ad1. Saving that produced a
    // recording that replayed in the session that made it and matched nothing
    // afterwards -- and every recording saved so far had it.
    const target = windowInfo({
      class_name: "WindowsForms10.Window.8.app.0.34473a7_r14_ad1",
      process_name: "fixture.exe",
      title: "Capture Fixture",
    });

    const selector = selectorFor(target, [target]);

    expect(selector?.class_name).toBeUndefined();
    // Something durable has to take its place, or the window is unfindable.
    expect(selector?.process_name).toBe("fixture.exe");
  });

  it("still uses a class name that survives a restart", () => {
    // The other half. Discarding every class name would weaken the selector for
    // the majority of windows: of twenty open on the test machine, only the
    // WinForms one was volatile.
    for (const className of ["Notepad", "Chrome_WidgetWin_1", "XLMAIN", "CabinetWClass"]) {
      const target = windowInfo({ class_name: className });
      expect(selectorFor(target, [target])?.class_name).toBe(className);
    }
  });

  it("falls through to a title when the class is volatile and the process is shared", () => {
    // Two windows of the same WinForms app: the class is useless, the process is
    // identical, so only the title is left.
    const target = windowInfo({
      window_id: "hwnd:1",
      class_name: "WindowsForms10.Window.8.app.0.aaa_r1_ad1",
      process_name: "app.exe",
      title: "First",
    });
    const rival = windowInfo({
      window_id: "hwnd:2",
      class_name: "WindowsForms10.Window.8.app.0.bbb_r2_ad1",
      process_name: "app.exe",
      title: "Second",
    });

    const selector = selectorFor(target, [target, rival]);

    expect(selector).toEqual({ process_name: "app.exe", title: "First" });
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

describe("assertions", () => {
  beforeEach(() => resetIds());

  it("attaches the check to the action instead of adding a step", () => {
    // A check that runs as its own step can report success after the action it
    // was meant to verify has already failed. Attaching it also keeps the
    // reference self-contained: deleting or disabling the step takes the check
    // with it, so `of_step` can never dangle.
    const recording = new Recording();
    const step = recording.add(draft());
    step.assertion = { mode: "exists", locator: { name: "Saved" } };

    const steps = recording.toDescriptor().steps as Record<string, unknown>[];

    expect(steps).toHaveLength(3);
    const action = steps[2];
    expect(action.id).toBe(step.id);
    const postcondition = action.postcondition as Record<string, unknown>;
    expect(postcondition.condition).toBe("${{ observation.found }}");
  });

  it("re-observes rather than reusing the snapshot the action was aimed at", () => {
    // The question is whether the screen changed. A snapshot captured before
    // the action cannot answer it, however convenient it is to reuse.
    const recording = new Recording();
    const step = recording.add(draft());
    step.assertion = { mode: "exists" };

    const steps = recording.toDescriptor().steps as Record<string, unknown>[];
    const postcondition = (steps[2] as Record<string, unknown>)
      .postcondition as Record<string, unknown>;
    const observe = postcondition.observe as Record<string, unknown>;
    const observeWith = observe.with as Record<string, unknown>;

    expect(observe.uses).toBe("desktop.windows_uia.find@1");
    expect(observeWith.snapshot_id).toBeUndefined();
  });

  it("asks find to tolerate a miss when checking something is gone", () => {
    // Without this the observation fails with a retryable error, which polling
    // reads as "not yet" -- so the assertion could never be satisfied, only
    // time out. Verified on a real window: absent passes in 0.16s with it.
    const recording = new Recording();
    const step = recording.add(draft());
    step.assertion = { mode: "absent", locator: { name: "Spinner" } };

    const steps = recording.toDescriptor().steps as Record<string, unknown>[];
    const postcondition = (steps[2] as Record<string, unknown>)
      .postcondition as Record<string, unknown>;
    const observeWith = (postcondition.observe as Record<string, unknown>)
      .with as Record<string, unknown>;

    expect(observeWith.expect).toBe("optional");
    expect(postcondition.condition).toBe("${{ not observation.found }}");
  });

  it("checks a value by containment, because the evaluator forbids calls", () => {
    // value_matches is deliberately a substring test, not a regular
    // expression: the evaluator rejects function and method calls outright, so
    // there is no matcher to invoke. Measured -- a regex call does not even
    // validate. Naming it a regex would be a lie the first real pattern finds.
    const recording = new Recording();
    const step = recording.add(draft({ action: "set_value" }));
    recording.setArgument(step.id, "typed");
    step.assertion = { mode: "value_matches", expected: "ype" };

    const steps = recording.toDescriptor().steps as Record<string, unknown>[];
    const postcondition = (steps[2] as Record<string, unknown>)
      .postcondition as Record<string, unknown>;

    expect(postcondition.condition).toBe('${{ "ype" in observation.node.value }}');
  });

  it("compares a state against a boolean, not the string the form collected", () => {
    // States arrive as booleans in an observation, so quoting the value would
    // compare a bool to a string and be false for every input.
    const recording = new Recording();
    const step = recording.add(draft());
    step.assertion = { mode: "state_equals", state: "enabled", expected: "true" };

    const steps = recording.toDescriptor().steps as Record<string, unknown>[];
    const postcondition = (steps[2] as Record<string, unknown>)
      .postcondition as Record<string, unknown>;

    expect(postcondition.condition).toBe(
      "${{ observation.node.states.enabled == True }}",
    );
  });

  it("falls back to the step's own element when the check names none", () => {
    // "Did my typing land?" is the common case and should not require
    // restating the locator that is already on the step.
    const recording = new Recording();
    const step = recording.add(draft({ action: "set_value" }));
    recording.setArgument(step.id, "text");
    step.assertion = { mode: "value_equals", expected: "text" };

    expect(recording.validate().filter((issue) => issue.blocking)).toEqual([]);

    const steps = recording.toDescriptor().steps as Record<string, unknown>[];
    const observeWith = (
      (steps[2] as Record<string, unknown>).postcondition as Record<string, unknown>
    );
    const target = (observeWith.observe as Record<string, unknown>)
      .with as Record<string, unknown>;
    expect(target.locator).toEqual(step.locator);
  });

  it("refuses a comparison with nothing to compare against", () => {
    // Compiling this anyway yields a comparison against the empty string: a
    // check that always fails while looking like it is working.
    const recording = new Recording();
    const step = recording.add(draft());
    step.assertion = { mode: "value_equals", expected: "" };

    const issues = recording.validate().filter((issue) => issue.blocking);

    expect(issues).toHaveLength(1);
    expect(issues[0].message).toContain("needs a value");
  });

  it("refuses a state that no observation ever carries", () => {
    // Referencing a missing field fails the whole run with an expression
    // error, not an assertion failure -- confirmed against a live window.
    const recording = new Recording();
    const step = recording.add(draft());
    step.assertion = { mode: "state_equals", state: "checked", expected: "true" };

    const issues = recording.validate().filter((issue) => issue.blocking);

    expect(issues).toHaveLength(1);
    expect(issues[0].message).toContain("no checked state");
  });

  it("keeps the check when the recording is saved and reopened", () => {
    // A recording that quietly loses its check still replays and still reports
    // success, having verified nothing.
    const recording = new Recording();
    const step = recording.add(draft());
    step.assertion = {
      mode: "value_equals",
      locator: { name: "Status" },
      expected: "Saved",
      timeout: "5s",
      pollInterval: "200ms",
    };

    const reopened = Recording.fromDocument(
      JSON.parse(JSON.stringify(recording.toDocument())),
    );

    expect(reopened.steps[0].assertion).toEqual({
      mode: "value_equals",
      locator: { name: "Status" },
      expected: "Saved",
      state: undefined,
      timeout: "5s",
      pollInterval: "200ms",
    });
  });

  it("writes of_step so the file matches the format others read", () => {
    const recording = new Recording();
    const step = recording.add(draft());
    step.assertion = { mode: "exists" };

    const document = recording.toDocument() as Record<string, unknown>;
    const saved = (document.steps as Record<string, unknown>[])[0];
    const assertion = saved.assertion as Record<string, unknown>;

    expect(assertion.of_step).toBe(step.id);
    expect(assertion.kind).toBe("assertion");
  });

  it("refuses to open a recording whose check it cannot understand", () => {
    // Dropping the unknown check would leave a recording that looks verified
    // and is not -- the exact outcome an assertion exists to prevent.
    const recording = new Recording();
    const step = recording.add(draft());
    step.assertion = { mode: "exists" };
    const document = JSON.parse(JSON.stringify(recording.toDocument()));
    document.steps[0].assertion.mode = "pixel_perfect";

    expect(() => Recording.fromDocument(document)).toThrow(/cannot check/);
  });

  it("takes the check away with the step it belongs to", () => {
    // Why the assertion lives on the step: the saved format's `of_step` is a
    // cross-step reference, and this arrangement makes a dangling one
    // unreachable rather than something to validate against.
    const recording = new Recording();
    const first = recording.add(draft());
    first.assertion = { mode: "exists" };
    recording.add(draft());

    recording.remove(first.id);

    const descriptor = JSON.stringify(recording.toDescriptor());
    expect(descriptor).not.toContain("postcondition");
  });
});

describe("editing a check", () => {
  beforeEach(() => resetIds());

  it("keeps the value already typed when only the mode changes", () => {
    // Choosing from a dropdown must not discard what is typed beside it: a
    // person switching between "is exactly" and "contains" is refining the
    // same thought, not starting over.
    const recording = new Recording();
    const step = recording.add(draft());
    recording.setAssertion(step.id, { mode: "value_equals", expected: "Saved" });

    recording.setAssertion(step.id, { mode: "value_matches" });

    expect(step.assertion).toMatchObject({ mode: "value_matches", expected: "Saved" });
  });

  it("drops a comparison value the new mode cannot use", () => {
    // Left in place it would be written to the saved file and reappear if the
    // mode changed back, reading as though it were in force when it is not.
    const recording = new Recording();
    const step = recording.add(draft());
    recording.setAssertion(step.id, { mode: "value_equals", expected: "Saved" });

    recording.setAssertion(step.id, { mode: "exists" });

    expect(step.assertion?.expected).toBeUndefined();
  });

  it("gives a state check a real flag rather than an empty one", () => {
    // Picking "the state becomes" from a dropdown should produce a check that
    // works, not a half-filled form that fails validation.
    const recording = new Recording();
    const step = recording.add(draft());

    recording.setAssertion(step.id, { mode: "state_equals" });

    expect(step.assertion?.state).toBe("enabled");
    expect(recording.validate().filter((issue) => issue.blocking)).toEqual([]);
  });

  it("forgets the state flag once the mode no longer reads one", () => {
    const recording = new Recording();
    const step = recording.add(draft());
    recording.setAssertion(step.id, { mode: "state_equals", state: "focused" });

    recording.setAssertion(step.id, { mode: "exists" });

    expect(step.assertion?.state).toBeUndefined();
  });

  it("removes the check when asked, leaving no trace in the output", () => {
    const recording = new Recording();
    const step = recording.add(draft());
    recording.setAssertion(step.id, { mode: "exists" });

    recording.setAssertion(step.id, null);

    expect(step.assertion).toBeUndefined();
    expect(JSON.stringify(recording.toDescriptor())).not.toContain("postcondition");
  });

  it("says so when the step is gone", () => {
    const recording = new Recording();

    expect(recording.setAssertion("step_404", { mode: "exists" })).toBe(false);
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

  it("still keeps a password out of the file after a round trip", () => {
    // Found by a failing round-trip assertion: the protected marking was not
    // being saved, so reopening a recording would start inlining a credential
    // that had correctly been externalised the first time.
    const recording = new Recording();
    const step = recording.add(
      draft({
        action: "type_text",
        argument: "hunter2-real-secret",
        element: element({ protected: true }),
      }),
    );

    const reopened = Recording.fromDocument(recording.toDocument());
    const descriptor = reopened.toDescriptor("fixed");
    const steps = descriptor.steps as Record<string, unknown>[];

    expect(reopened.steps[0].protected).toBe(true);
    expect((steps[2].with as Record<string, unknown>).text).toBe(
      `\${{ inputs.${step.id}_secret }}`,
    );
    expect(JSON.stringify(descriptor)).not.toContain("hunter2-real-secret");
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

// ---------------------------------------------------------------------------
// Adopting steps from a capture session
//
// The failure to guard against is a recording that looks complete and replays
// something else: an interaction silently dropped, or a step that cannot locate
// its element marked ready to run.
// ---------------------------------------------------------------------------

describe("addCaptured", () => {
  const windowOf = (over: Partial<WindowInfo> = {}): WindowInfo => ({
    window_id: "hwnd:1",
    title: "Fixture",
    process_id: 10,
    process_name: "fixture.exe",
    class_name: "FixtureClass",
    bounds: null,
    is_foreground: true,
    is_minimized: false,
    ...over,
  });

  const step = (over: Partial<CapturedStep> = {}): CapturedStep => ({
    action: "invoke",
    locator: { role: "button", name: "Submit" },
    summary: "button Submit",
    argument: null,
    protected: false,
    replayable: true,
    unresolved: null,
    ...over,
  });

  beforeEach(() => resetIds());

  it("adopts a captured step as a runnable one", () => {
    const recording = new Recording();
    const added = recording.addCaptured([step()], windowOf());

    expect(added).toHaveLength(1);
    expect(added[0].enabled).toBe(true);
    expect(added[0].locator).toEqual({ role: "button", name: "Submit" });
    // The window selector is built here, not taken from the capture: capture
    // says which window, not how to find it next time.
    expect(added[0].window).not.toBeNull();
  });

  it("keeps a step it cannot replay, disabled, instead of dropping it", () => {
    // Dropping it would leave a recording that looks complete while missing an
    // interaction the user performed -- discovered at replay, long after the
    // session that could explain it ended. Correcting a recording is only
    // possible for problems a person can see.
    const recording = new Recording();
    const added = recording.addCaptured(
      [step({ locator: null, replayable: false, unresolved: "cannot be told apart" })],
      windowOf(),
    );

    expect(recording.steps).toHaveLength(1);
    expect(added[0].enabled).toBe(false);
  });

  it("refuses to enable a step whose element cannot be located", () => {
    // The recording must not be talked into running something that will fail.
    const recording = new Recording();
    const added = recording.addCaptured([step({ locator: null })], windowOf());

    expect(recording.setEnabled(added[0].id, true)).toBe(false);
    expect(added[0].enabled).toBe(false);
  });

  it("never writes a class name the toolkit regenerates into the selector", () => {
    // Measured: WinForms rebuilds this per run, so a recording saved with it
    // replays in the session that made it and fails ever after.
    const recording = new Recording();
    const target = windowOf({
      class_name: "WindowsForms10.Window.8.app.0.34473a7_r14_ad1",
    });
    const added = recording.addCaptured([step()], target, [target]);

    expect(added[0].window?.class_name).toBeUndefined();
    // And it still found some way to identify the window.
    expect(added[0].window).not.toBeNull();
  });

  it("disables every step when two windows of the app cannot be told apart", () => {
    // The driver refuses an ambiguous window selector rather than picking one,
    // so enabling these would produce a workflow that cannot run.
    const recording = new Recording();
    const first = windowOf({ window_id: "hwnd:1", title: "Same" });
    const second = windowOf({ window_id: "hwnd:2", title: "Same" });
    const added = recording.addCaptured([step()], first, [first, second]);

    expect(added[0].window).toBeNull();
    expect(added[0].enabled).toBe(false);
  });

  it("carries the typed text through and keeps a protected value out", () => {
    const recording = new Recording();
    const added = recording.addCaptured(
      [
        step({ action: "set_value", argument: "Ada", locator: { name: "NameBox" } }),
        step({
          action: "set_value",
          argument: null,
          protected: true,
          locator: { name: "PasswordBox" },
        }),
      ],
      windowOf(),
    );

    expect(added[0].argument).toBe("Ada");
    expect(added[1].argument).toBeUndefined();
    expect(added[1].protected).toBe(true);
    // A login step still has to be usable, or automating one is impossible.
    expect(added[1].enabled).toBe(true);
  });

  it("re-enables a step once its locator is corrected", () => {
    // The point of correcting a locator. Leaving the step disabled would make
    // the fix appear to work while the compiled workflow silently omits it.
    const recording = new Recording();
    const added = recording.addCaptured([step({ locator: null })], windowOf());
    expect(added[0].enabled).toBe(false);

    expect(recording.setLocator(added[0].id, { role: "button", nth: 3 })).toBe(true);

    expect(added[0].enabled).toBe(true);
    expect(added[0].locator).toEqual({ role: "button", nth: 3 });
    expect(recording.enabledSteps).toHaveLength(1);
  });

  it("keeps a step disabled when its window is still ambiguous", () => {
    // A locator is only half of what a step needs. Enabling it on the strength
    // of the locator alone would produce a workflow the driver refuses.
    const recording = new Recording();
    const first = windowOf({ window_id: "hwnd:1", title: "Same" });
    const second = windowOf({ window_id: "hwnd:2", title: "Same" });
    const added = recording.addCaptured([step({ locator: null })], first, [first, second]);

    recording.setLocator(added[0].id, { role: "button" });

    expect(added[0].window).toBeNull();
    expect(added[0].enabled).toBe(false);
  });

  it("carries a descriptive locator through save and reopen", () => {
    // A corrected locator is worth nothing if the file loses the parts the
    // recorder never produced -- position and proximity are exactly those.
    const recording = new Recording();
    const added = recording.addCaptured([step()], windowOf());
    recording.setLocator(added[0].id, {
      role: "edit",
      states: { focusable: true },
      near: { anchor: { name: "Name:", role: "text" }, direction: "right", within: 40 },
    });

    const reopened = Recording.fromDocument(recording.toDocument());

    expect(reopened.steps[0].locator).toEqual({
      role: "edit",
      states: { focusable: true },
      near: { anchor: { name: "Name:", role: "text" }, direction: "right", within: 40 },
    });
    expect(reopened.steps[0].enabled).toBe(true);
  });

  it("compiles a corrected step into the workflow", () => {
    // Crossing the whole path: a correction that never reaches the descriptor
    // has not fixed anything.
    const recording = new Recording();
    const added = recording.addCaptured([step({ locator: null })], windowOf());
    recording.setLocator(added[0].id, { role: "button", nth: "last" });

    const descriptor = recording.toDescriptor("fixed") as Record<string, unknown>;
    const steps = descriptor.steps as Record<string, unknown>[];
    const find = steps.find((entry) => String(entry.id).endsWith("_element"));

    expect((find?.with as Record<string, unknown>)?.locator).toEqual({
      role: "button",
      nth: "last",
    });
  });

  it("falls back to the title when the process name alone is shared", () => {
    // Observed while verifying this: a recording came out with just
    // {process_name: "powershell.exe"}, which replayed only because a single
    // such window happened to be left open. Two were open minutes earlier.
    const recording = new Recording();
    const target = windowOf({
      window_id: "hwnd:1",
      title: "Fixture | clicks=0",
      class_name: "WindowsForms10.Window.8.app.0.34473a7_r14_ad1",
    });
    const other = windowOf({
      window_id: "hwnd:2",
      title: "Something else",
      class_name: "WindowsForms10.Window.8.app.0.376a1c9_r8_ad1",
    });
    const added = recording.addCaptured([step()], target, [target, other]);

    // The class name is volatile so it cannot help, and the process name is
    // shared, so the title is all that is left.
    expect(added[0].window?.title).toBe("Fixture | clicks=0");
    expect(added[0].window?.class_name).toBeUndefined();
    expect(added[0].enabled).toBe(true);
  });

  it("appends to what is already there rather than replacing it", () => {
    // Polling adds a batch at a time, so a second batch must not discard the
    // first -- and the ids must not collide, because they become workflow step
    // ids.
    const recording = new Recording();
    recording.addCaptured([step()], windowOf());
    recording.addCaptured([step({ summary: "button Reset" })], windowOf());

    expect(recording.steps).toHaveLength(2);
    expect(new Set(recording.steps.map((entry) => entry.id)).size).toBe(2);
  });

  it("produces steps that survive a save and reopen", () => {
    // The whole point of a locator over a reference: a captured step has to
    // still work after the file has been closed and opened again.
    const recording = new Recording();
    recording.addCaptured(
      [step({ action: "set_value", argument: "Ada", locator: { name: "NameBox" } })],
      windowOf(),
    );

    const reopened = Recording.fromDocument(recording.toDocument());

    expect(reopened.steps).toHaveLength(1);
    expect(reopened.steps[0].enabled).toBe(true);
    expect(reopened.steps[0].argument).toBe("Ada");
    expect(reopened.enabledSteps).toHaveLength(1);
  });

  it("compiles adopted steps into a runnable workflow", () => {
    // Adopting a step is only useful if it reaches a descriptor the engine
    // accepts, so this crosses the whole path rather than stopping at the model.
    const recording = new Recording();
    recording.addCaptured(
      [step({ action: "set_value", argument: "Ada", locator: { name: "NameBox" } })],
      windowOf(),
    );

    const descriptor = recording.toDescriptor("adopted") as Record<string, unknown>;
    const steps = descriptor.steps as Record<string, unknown>[];

    // snapshot, find, act
    expect(steps).toHaveLength(3);
    expect(steps[2].uses).toBe("desktop.windows_uia.set_value@1");
    expect((steps[2].with as Record<string, unknown>).value).toBe("Ada");
  });
});
