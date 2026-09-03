/**
 * The recording being composed, and the rules that keep it coherent.
 *
 * This is the replacement for the Python browser editor. It is deliberately
 * free of Vue and of the Tauri bridge so that every editing rule can be tested
 * directly, which is exactly the property the Python module was built for.
 *
 * The central constraint, verified against a running desktop rather than assumed:
 * a `snapshot:revision:node` reference stops resolving once its snapshot is gone
 * (`DRIVER.STALE_HANDLE` after clearing the snapshot store). A recording exists
 * to be saved and replayed later, so a step stores a **locator** — a description
 * of the element — and the compiled workflow re-finds the element at replay time.
 */

import type { Element, Locator, WindowInfo } from "./bridge";

/** Actions that need a text argument to be meaningful. */
export const ACTIONS_NEEDING_TEXT = ["set_value", "type_text"] as const;

/** The format identifiers a saved recording carries. */
export const RECORDING_API_VERSION = "ai-auto-desktop.dev/v1alpha1";
export const RECORDING_KIND = "Recording";
export const WORKFLOW_KIND = "Workflow";

export interface Step {
  id: string;
  action: string;
  /**
   * How to find the element at replay time. Null means the recorder could not
   * tell this element apart from its siblings, so the step cannot be replayed
   * and must be resolved by a human before the recording compiles.
   */
  locator: Locator | null;
  /** A human-readable description of the element, for display only. */
  summary: string;
  /**
   * How to find the window at replay time, or null when the window could not be
   * told apart from another one open at the time.
   */
  window: WindowSelector | null;
  /** The window's title when recorded, for display only. */
  windowTitle: string;
  argument?: string;
  /**
   * The element masks its content, as a password field does.
   *
   * Kept on the step because it decides whether `argument` may be written into
   * the exported file. A recording is a file people copy and share, so a secret
   * typed here is externalised as a workflow input instead of being baked in.
   */
  protected?: boolean;
  enabled: boolean;
  /**
   * What must become true for this step to count as having worked.
   *
   * Held on the step rather than as a separate entry that points back at one.
   * The saved format names its subject with `of_step`, which is a cross-step
   * reference, and this model is otherwise provably incapable of producing a
   * dangling one -- every reference it emits is derived inside a single step's
   * own expansion. Keeping the assertion attached preserves that: removing,
   * disabling or moving a step takes its assertion with it.
   */
  assertion?: Assertion;
}

/** The five ways a recording can state what an action was supposed to achieve. */
export type AssertionMode =
  | "exists"
  | "absent"
  | "value_equals"
  | "value_matches"
  | "state_equals";

export interface Assertion {
  mode: AssertionMode;
  /**
   * What to look at. Defaults to the step's own element, which is what
   * "did my typing land?" means; give a locator to check something else, such
   * as the dialog a button opened.
   */
  locator?: Locator | null;
  /** Required by value_equals, value_matches and state_equals. */
  expected?: string;
  /** Which state flag state_equals reads, such as `enabled` or `focused`. */
  state?: string;
  /** How long to keep re-checking. Absent means check once. */
  timeout?: string;
  pollInterval?: string;
}

/** The state flags an observation actually carries, so a mode cannot name one that never arrives. */
export const ASSERTABLE_STATES = [
  "enabled",
  "offscreen",
  "focusable",
  "focused",
  "read_only",
  "protected",
] as const;

/** Modes that compare against `expected`. */
const MODES_NEEDING_EXPECTED: readonly AssertionMode[] = [
  "value_equals",
  "value_matches",
  "state_equals",
];

export const ASSERTION_MODES: readonly AssertionMode[] = [
  "exists",
  "absent",
  "value_equals",
  "value_matches",
  "state_equals",
];

/** How to find the window again, without depending on a live handle. */
export interface WindowSelector {
  title?: string;
  process_name?: string;
  class_name?: string;
}

/**
 * Class names that are regenerated every time the program starts.
 *
 * WinForms builds one per run, so the same window reports
 * `WindowsForms10.Window.8.app.0.34473a7_r14_ad1` on one launch and
 * `...0.376a1c9_r8_ad1` on the next -- measured on this machine by restarting a
 * fixture. Saving one produces a recording that replays perfectly in the session
 * that made it and matches nothing afterwards, which is the worst way for this
 * to fail: it looks correct exactly when it is being tested.
 *
 * Deliberately narrow. It matches the one shape that has actually been observed
 * changing; of the twenty windows open on this machine only the WinForms one is
 * caught, and discarding a stable class name would weaken the selector for no
 * reason.
 */
const VOLATILE_CLASS_NAME = /_r\d+_ad\d+$/;

/** Whether this class name will still mean the same thing after a restart. */
export function isDurableClassName(className: string | null): boolean {
  return Boolean(className) && !VOLATILE_CLASS_NAME.test(className as string);
}

/**
 * Build the narrowest selector that picks `target` out of `open`.
 *
 * Same discipline as element locators, for the same reason: the driver treats an
 * ambiguous window selector as a failure rather than choosing one, so a selector
 * that matched two windows at record time would simply refuse to replay.
 *
 * Ordered by stability. A class name outlives editing -- unless the toolkit
 * regenerates it per run, in which case it is skipped entirely; a process name is
 * stable but shared by every window of the app; a title is the least durable
 * because it changes as soon as the document is renamed or modified, so it is
 * used only when nothing else separates the windows.
 *
 * Returns null when even a title cannot distinguish the window, which is a real
 * situation the caller has to surface rather than paper over.
 */
export function selectorFor(
  target: WindowInfo,
  open: WindowInfo[] = [target],
): WindowSelector | null {
  const pool = open.length ? open : [target];
  const matches = (selector: WindowSelector, candidate: WindowInfo): boolean =>
    (selector.class_name === undefined || candidate.class_name === selector.class_name) &&
    (selector.process_name === undefined ||
      (candidate.process_name ?? "").toLowerCase() ===
        selector.process_name.toLowerCase()) &&
    // Substring, mirroring how the driver compares titles.
    (selector.title === undefined || candidate.title.includes(selector.title));

  const isUnique = (selector: WindowSelector): boolean => {
    const hits = pool.filter((candidate) => matches(selector, candidate));
    return hits.length === 1 && hits[0].window_id === target.window_id;
  };

  const selector: WindowSelector = {};
  if (isDurableClassName(target.class_name)) {
    selector.class_name = target.class_name as string;
  }
  if (isUnique(selector) && Object.keys(selector).length) {
    return selector;
  }

  if (target.process_name) {
    selector.process_name = target.process_name;
  }
  if (isUnique(selector) && Object.keys(selector).length) {
    return selector;
  }

  if (target.title) {
    selector.title = target.title;
  }
  return isUnique(selector) && Object.keys(selector).length ? selector : null;
}

export interface StepDraft {
  action: string;
  element: Element;
  window: WindowInfo;
  argument?: string;
  /**
   * Every window open at record time.
   *
   * Needed because a selector has to be checked for uniqueness against its
   * competition: two windows of the same application share a class and a
   * process, and the driver refuses an ambiguous selector rather than picking
   * one, so a recording made without looking at the alternatives would fail to
   * replay whenever a second window of the app happens to be open.
   */
  openWindows?: WindowInfo[];
}

export interface ValidationIssue {
  stepId: string;
  message: string;
  /** True when the step can never replay, as opposed to merely needing input. */
  blocking: boolean;
}

let counter = 0;

/** Reset the id sequence, for deterministic tests. */
export function resetIds(): void {
  counter = 0;
}

function nextId(): string {
  counter += 1;
  return `step_${counter}`;
}

export class Recording {
  steps: Step[] = [];
  name = "recorded.workflow";

  /** The steps that would actually run, in order. */
  get enabledSteps(): Step[] {
    return this.steps.filter((step) => step.enabled);
  }

  add(draft: StepDraft): Step {
    const window = selectorFor(draft.window, draft.openWindows ?? [draft.window]);
    const step: Step = {
      id: nextId(),
      action: draft.action,
      locator: draft.element.locator,
      summary: draft.element.summary,
      window,
      windowTitle: draft.window.title,
      argument: draft.argument,
      protected: draft.element.protected === true,
      // A step that cannot locate its element or its window would fail at
      // replay, so it is recorded but left out of the run until a human
      // resolves it.
      enabled: draft.element.locator !== null && window !== null,
    };
    this.steps.push(step);
    return step;
  }

  remove(stepId: string): boolean {
    const before = this.steps.length;
    this.steps = this.steps.filter((step) => step.id !== stepId);
    return this.steps.length < before;
  }

  /** Disable a step without losing it, so a recording can be tried both ways. */
  setEnabled(stepId: string, enabled: boolean): boolean {
    const step = this.steps.find((candidate) => candidate.id === stepId);
    if (!step) {
      return false;
    }
    // Enabling a step that cannot be located would produce a workflow that
    // fails at replay, so the recording refuses rather than allowing it.
    if (enabled && (step.locator === null || step.window === null)) {
      return false;
    }
    step.enabled = enabled;
    return true;
  }

  setArgument(stepId: string, argument: string): boolean {
    const step = this.steps.find((candidate) => candidate.id === stepId);
    if (!step) {
      return false;
    }
    step.argument = argument;
    return true;
  }

  /**
   * Attach, change or remove a step's check.
   *
   * Passing null removes it. Partial updates merge into what is already there,
   * so changing the mode from a dropdown does not silently discard the value
   * or timeout typed beside it.
   */
  setAssertion(stepId: string, assertion: Partial<Assertion> | null): boolean {
    const step = this.steps.find((candidate) => candidate.id === stepId);
    if (!step) {
      return false;
    }
    if (assertion === null) {
      delete step.assertion;
      return true;
    }
    const merged: Assertion = {
      ...(step.assertion ?? { mode: "exists" }),
      ...assertion,
    };
    // A mode that takes no comparison value must not keep a stale one: it would
    // be written into the saved file, reappear if the mode changed back, and
    // read as though it were in force when it is not.
    if (!MODES_NEEDING_EXPECTED.includes(merged.mode)) {
      delete merged.expected;
    }
    if (merged.mode !== "state_equals") {
      delete merged.state;
    } else {
      // Default both halves, not just the flag. The editor's dropdowns fall
      // back to `enabled` and `true` for display, so leaving either unset
      // shows a filled-in form backed by a check that fails validation.
      merged.state ??= "enabled";
      merged.expected ??= "true";
    }
    step.assertion = merged;
    return true;
  }

  /**
   * Move a step to a new position.
   *
   * Reordering is allowed even when it looks questionable: refusing would
   * block a deliberate restructure. Problems surface through `validate`.
   */
  move(stepId: string, toIndex: number): boolean {
    const from = this.steps.findIndex((step) => step.id === stepId);
    if (from < 0 || toIndex < 0 || toIndex >= this.steps.length) {
      return false;
    }
    const [step] = this.steps.splice(from, 1);
    this.steps.splice(toIndex, 0, step);
    return true;
  }

  /** Problems that would make the recording fail or behave surprisingly. */
  validate(): ValidationIssue[] {
    const issues: ValidationIssue[] = [];
    for (const step of this.steps) {
      // An unlocatable step is worth reporting even while disabled, because it
      // is the reason the step is disabled.
      if (step.locator === null) {
        issues.push({
          stepId: step.id,
          message: "this element cannot be told apart from its siblings, so it cannot replay",
          blocking: true,
        });
        continue;
      }
      if (step.window === null) {
        issues.push({
          stepId: step.id,
          // Two windows of the same app were open, and the driver refuses an
          // ambiguous selector rather than picking one.
          message:
            "this window cannot be told apart from another one that was open, " +
            "so it cannot replay; close the duplicate and record again",
          blocking: true,
        });
        continue;
      }
      if (!step.enabled) {
        continue;
      }
      if (
        (ACTIONS_NEEDING_TEXT as readonly string[]).includes(step.action) &&
        !step.argument &&
        // A protected step is not missing anything: its value is supplied at run
        // time as a workflow input and is deliberately absent from the file.
        !step.protected
      ) {
        issues.push({
          stepId: step.id,
          message: `${step.action} needs text to enter`,
          blocking: true,
        });
      }
      const assertion = step.assertion;
      if (assertion) {
        if (!ASSERTION_MODES.includes(assertion.mode)) {
          issues.push({
            stepId: step.id,
            message: `${assertion.mode} is not something this can check`,
            blocking: true,
          });
        } else if (
          MODES_NEEDING_EXPECTED.includes(assertion.mode) &&
          (assertion.expected ?? "") === ""
        ) {
          // Compiling this anyway would produce a comparison against the empty
          // string: a check that always fails while looking like it is working.
          issues.push({
            stepId: step.id,
            message: `${assertion.mode} needs a value to compare against`,
            blocking: true,
          });
        }
        if (
          assertion.mode === "state_equals" &&
          assertion.state !== undefined &&
          !(ASSERTABLE_STATES as readonly string[]).includes(assertion.state)
        ) {
          // A state that never arrives in an observation would make the
          // condition reference a missing field, which fails the whole run with
          // an expression error rather than an assertion failure.
          issues.push({
            stepId: step.id,
            message: `there is no ${assertion.state} state to check`,
            blocking: true,
          });
        }
        // An assertion with no locator of its own falls back to the step's
        // element, which is what "did my typing land?" means. Only when
        // neither supplies one is there nothing to look at.
        if (!(assertion.locator ?? step.locator)) {
          issues.push({
            stepId: step.id,
            message: "this check has no element to look at",
            blocking: true,
          });
        }
      }
    }
    if (this.enabledSteps.length === 0) {
      issues.push({
        stepId: "",
        // A workflow with no steps is rejected by the compiler, so say so here
        // rather than letting export produce something invalid.
        message: "a recording needs at least one enabled step",
        blocking: true,
      });
    }
    return issues;
  }

  /** Whether this recording can be exported at all. */
  get canExport(): boolean {
    return this.validate().every((issue) => !issue.blocking);
  }

  /**
   * The saved form of this recording: the source a human reopens and edits.
   *
   * Distinct from the compiled workflow, and deliberately so. Compilation is
   * one-way, because a workflow can express far more than a recording can
   * represent, so round-tripping through it would lose the parts a person
   * edits.
   */
  toDocument(): Record<string, unknown> {
    return {
      apiVersion: RECORDING_API_VERSION,
      kind: RECORDING_KIND,
      metadata: { name: this.name },
      steps: this.steps.map((step) => ({
        id: step.id,
        action: step.action,
        locator: step.locator,
        summary: step.summary,
        window: step.window,
        window_title: step.windowTitle,
        // A protected step's text is deliberately dropped here as well as from
        // the compiled workflow. Externalising it from one file and writing it
        // verbatim into the other, in the same directory, would keep the
        // credential on disk while looking like it had been handled.
        ...(step.argument === undefined || step.protected
          ? {}
          : { argument: step.argument }),
        // Persisted because it decides whether this step's text may be written
        // into the compiled workflow. Losing it on reopen would silently start
        // inlining a credential that was correctly externalised before.
        protected: step.protected === true,
        enabled: step.enabled,
        // Written in the spec's shape, including `of_step`, even though the
        // model holds it on the step. The format is the contract with anything
        // else that reads these files; the in-memory arrangement is not.
        ...(step.assertion
          ? {
              assertion: {
                of_step: step.id,
                kind: "assertion",
                mode: step.assertion.mode,
                ...(step.assertion.locator ? { locator: step.assertion.locator } : {}),
                ...(step.assertion.expected === undefined
                  ? {}
                  : { expected: step.assertion.expected }),
                ...(step.assertion.state === undefined
                  ? {}
                  : { state: step.assertion.state }),
                ...(step.assertion.timeout ? { timeout: step.assertion.timeout } : {}),
                ...(step.assertion.pollInterval
                  ? { poll_interval: step.assertion.pollInterval }
                  : {}),
              },
            }
          : {}),
      })),
    };
  }

  /**
   * Rebuild a recording from its saved form.
   *
   * Rejects anything it does not recognise instead of silently dropping it: a
   * partially-loaded recording that still looks complete is how someone replays
   * fewer steps than they saved.
   */
  static fromDocument(document: unknown): Recording {
    if (!document || typeof document !== "object") {
      throw new Error("a recording file must contain an object");
    }
    const source = document as Record<string, unknown>;
    if (source.apiVersion !== RECORDING_API_VERSION) {
      throw new Error(
        `unsupported apiVersion ${JSON.stringify(source.apiVersion)}; ` +
          `expected ${RECORDING_API_VERSION}`,
      );
    }
    if (source.kind !== RECORDING_KIND) {
      throw new Error(
        `${JSON.stringify(source.kind)} is not a recording; expected ${RECORDING_KIND}`,
      );
    }
    if (!Array.isArray(source.steps)) {
      throw new Error("a recording must have a list of steps");
    }

    const recording = new Recording();
    const metadata = (source.metadata ?? {}) as Record<string, unknown>;
    if (typeof metadata.name === "string" && metadata.name) {
      recording.name = metadata.name;
    }

    const seen = new Set<string>();
    recording.steps = source.steps.map((raw, index) => {
      if (!raw || typeof raw !== "object") {
        throw new Error(`step ${index + 1} is not an object`);
      }
      const step = raw as Record<string, unknown>;
      if (typeof step.id !== "string" || !step.id) {
        throw new Error(`step ${index + 1} has no id`);
      }
      if (seen.has(step.id)) {
        // Duplicate ids would collide in the compiled workflow.
        throw new Error(`step id ${JSON.stringify(step.id)} appears more than once`);
      }
      seen.add(step.id);
      if (typeof step.action !== "string" || !step.action) {
        throw new Error(`step ${JSON.stringify(step.id)} has no action`);
      }
      const window = (step.window ?? null) as WindowSelector | null;
      return {
        id: step.id,
        action: step.action,
        locator: (step.locator ?? null) as Locator | null,
        summary: typeof step.summary === "string" ? step.summary : step.action,
        window,
        windowTitle: typeof step.window_title === "string" ? step.window_title : "",
        argument: typeof step.argument === "string" ? step.argument : undefined,
        protected: step.protected === true,
        assertion: readAssertion(step.assertion, step.id),
        // Never re-enable a step that cannot be located, whatever the file says.
        enabled:
          step.enabled !== false && (step.locator ?? null) !== null && window !== null,
      };
    });

    // Keep generated ids from colliding with loaded ones.
    for (const step of recording.steps) {
      const match = /^step_(\d+)$/.exec(step.id);
      if (match) {
        counter = Math.max(counter, Number(match[1]));
      }
    }
    return recording;
  }

  /**
   * Compile to a workflow descriptor.
   *
   * Each recorded action becomes three steps: capture a snapshot, find the
   * element by its locator, then act on what was found. That expansion is
   * forced by the driver's contract — an action needs a reference valid in the
   * current session, and only a locator survives being saved — and it is what
   * makes a reopened recording replayable at all.
   *
   * Text typed into an ordinary field is written into the file as-is: it is
   * usually the point of the recording, and hiding it would make a saved
   * workflow impossible to read back. Text typed into a *protected* field is
   * different — it is a credential, and a recording is a file people copy,
   * commit and share. Those become required workflow inputs, so the value is
   * supplied at run time and never lands on disk.
   */
  toDescriptor(name = this.name): Record<string, unknown> {
    const steps: Record<string, unknown>[] = [];
    const inputs: Record<string, unknown> = {};

    for (const step of this.enabledSteps) {
      const snapshotId = `${step.id}_window`;
      const findId = `${step.id}_element`;

      steps.push({
        id: snapshotId,
        type: "action",
        uses: "desktop.windows_uia.snapshot@1",
        with: { window: step.window },
      });

      steps.push({
        id: findId,
        type: "action",
        uses: "desktop.windows_uia.find@1",
        with: {
          snapshot_id: `\${{ steps.${snapshotId}.output.snapshot_id }}`,
          locator: step.locator,
        },
      });

      const args: Record<string, unknown> = {
        // Consume the reference the find step just produced, not a saved one.
        target: `\${{ steps.${findId}.output.ref }}`,
      };
      const needsText = (ACTIONS_NEEDING_TEXT as readonly string[]).includes(step.action);
      if (needsText) {
        const field = step.action === "set_value" ? "value" : "text";
        if (step.protected) {
          // Named after the step so two credentials in one recording stay
          // separate, and marked required so a missing one fails at the start
          // rather than halfway through a login.
          const inputName = `${step.id}_secret`;
          inputs[inputName] = {
            schema: { type: "string" },
            required: true,
            sensitive: true,
          };
          args[field] = `\${{ inputs.${inputName} }}`;
        } else {
          args[field] = step.argument ?? "";
        }
      }

      const action: Record<string, unknown> = {
        id: step.id,
        type: "action",
        uses: `desktop.windows_uia.${step.action}@1`,
        with: args,
      };

      // Attached to the action, never a step of its own. A check that runs as
      // a separate step can report success after the action it was meant to
      // verify has already failed, which is worse than having no check at all.
      const postcondition = compileAssertion(step, snapshotId);
      if (postcondition) {
        action.postcondition = postcondition;
      }

      steps.push(action);
    }

    const descriptor: Record<string, unknown> = {
      apiVersion: RECORDING_API_VERSION,
      kind: WORKFLOW_KIND,
      metadata: { name },
      budgets: {
        max_duration: "5m",
        // One recorded action costs three executed steps, so the budget has to
        // be set from the expanded count or a valid recording would run out.
        max_executed_steps: Math.max(10, steps.length * 2),
      },
      steps,
    };
    // Only present when something was actually externalised, so an ordinary
    // recording keeps the shape it had before.
    if (Object.keys(inputs).length) {
      descriptor.inputs = inputs;
    }
    return descriptor;
  }
}

/**
 * Rebuild an assertion from its saved form.
 *
 * Refuses an unknown mode rather than dropping it. A recording that quietly
 * loses its check still replays, reports success, and verifies nothing -- the
 * one outcome an assertion exists to prevent.
 */
function readAssertion(raw: unknown, stepId: string): Assertion | undefined {
  if (raw === undefined || raw === null) {
    return undefined;
  }
  if (typeof raw !== "object") {
    throw new Error(`step ${JSON.stringify(stepId)} has a malformed assertion`);
  }
  const source = raw as Record<string, unknown>;
  const mode = source.mode;
  if (typeof mode !== "string" || !ASSERTION_MODES.includes(mode as AssertionMode)) {
    throw new Error(
      `step ${JSON.stringify(stepId)} has an assertion this version cannot check: ` +
        JSON.stringify(mode),
    );
  }
  return {
    mode: mode as AssertionMode,
    locator: (source.locator ?? null) as Locator | null,
    expected: typeof source.expected === "string" ? source.expected : undefined,
    state: typeof source.state === "string" ? source.state : undefined,
    timeout: typeof source.timeout === "string" ? source.timeout : undefined,
    pollInterval:
      typeof source.poll_interval === "string" ? source.poll_interval : undefined,
  };
}

/** Quote a string for embedding in a condition expression. */
function quote(value: string): string {
  return JSON.stringify(value);
}

/**
 * Turn a step's assertion into a postcondition, or null when it has none.
 *
 * The observation always re-runs `find` rather than reusing the snapshot the
 * action was aimed at: the whole question is whether the screen changed, and a
 * snapshot taken before the action cannot answer it.
 */
function compileAssertion(
  step: Step,
  snapshotId: string,
): Record<string, unknown> | null {
  const assertion = step.assertion;
  if (!assertion) {
    return null;
  }

  const locator = assertion.locator ?? step.locator;
  const observeWith: Record<string, unknown> = {
    window: step.window,
    locator,
  };

  let condition: string;
  switch (assertion.mode) {
    case "exists":
      condition = "${{ observation.found }}";
      break;
    case "absent":
      // Without `optional` a miss is a retryable error, which polling reads as
      // "not yet" -- so this assertion could never be satisfied, only time out.
      observeWith.expect = "optional";
      condition = "${{ not observation.found }}";
      break;
    case "value_equals":
      condition = `\${{ observation.node.value == ${quote(assertion.expected ?? "")} }}`;
      break;
    case "value_matches":
      // Substring, not a regular expression. The evaluator refuses function and
      // method calls, so there is no matcher to call; `in` is the only
      // containment the grammar has.
      condition = `\${{ ${quote(assertion.expected ?? "")} in observation.node.value }}`;
      break;
    case "state_equals": {
      const state = assertion.state ?? "enabled";
      // States are booleans in an observation, so compare against one rather
      // than the string the UI collected.
      const wanted = assertion.expected === "false" ? "False" : "True";
      condition = `\${{ observation.node.states.${state} == ${wanted} }}`;
      break;
    }
  }

  const postcondition: Record<string, unknown> = {
    condition,
    observe: {
      uses: "desktop.windows_uia.find@1",
      with: observeWith,
    },
    message: describeAssertion(assertion, step),
  };
  if (assertion.timeout) {
    postcondition.timeout = assertion.timeout;
  }
  if (assertion.pollInterval) {
    postcondition.poll_interval = assertion.pollInterval;
  }
  // Unused today but part of the saved format: `snapshotId` names the capture
  // this action was aimed at, which is what an editor shows beside a failure.
  void snapshotId;
  return postcondition;
}

/** A sentence a person reads when the assertion fails. */
export function describeAssertion(assertion: Assertion, step: Step): string {
  const what = assertion.locator ? "the element it checks" : step.summary || "the element";
  switch (assertion.mode) {
    case "exists":
      return `expected ${what} to be present after ${step.action}`;
    case "absent":
      return `expected ${what} to be gone after ${step.action}`;
    case "value_equals":
      return `expected ${what} to read ${quote(assertion.expected ?? "")}`;
    case "value_matches":
      return `expected ${what} to contain ${quote(assertion.expected ?? "")}`;
    case "state_equals":
      return `expected ${what} to be ${assertion.state ?? "enabled"}=${
        assertion.expected ?? "true"
      }`;
  }
}
