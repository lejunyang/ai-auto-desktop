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
  enabled: boolean;
}

/** How to find the window again, without depending on a live handle. */
export interface WindowSelector {
  title?: string;
  process_name?: string;
  class_name?: string;
}

/**
 * Build the narrowest selector that picks `target` out of `open`.
 *
 * Same discipline as element locators, for the same reason: the driver treats an
 * ambiguous window selector as a failure rather than choosing one, so a selector
 * that matched two windows at record time would simply refuse to replay.
 *
 * Ordered by stability. A class name outlives editing; a process name is stable
 * but shared by every window of the app; a title is the least durable because it
 * changes as soon as the document is renamed or modified, so it is used only when
 * nothing else separates the windows.
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
  if (target.class_name) {
    selector.class_name = target.class_name;
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
        !step.argument
      ) {
        issues.push({
          stepId: step.id,
          message: `${step.action} needs text to enter`,
          blocking: true,
        });
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
        ...(step.argument === undefined ? {} : { argument: step.argument }),
        enabled: step.enabled,
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
   */
  toDescriptor(name = this.name): Record<string, unknown> {
    const steps: Record<string, unknown>[] = [];

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
      if (step.action === "set_value") {
        args.value = step.argument ?? "";
      }
      if (step.action === "type_text") {
        args.text = step.argument ?? "";
      }

      steps.push({
        id: step.id,
        type: "action",
        uses: `desktop.windows_uia.${step.action}@1`,
        with: args,
      });
    }

    return {
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
  }
}
