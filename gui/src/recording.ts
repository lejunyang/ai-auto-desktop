/**
 * The recording being composed, and the rules that keep it coherent.
 *
 * This is the replacement for the Python browser editor. It is deliberately
 * free of Vue and of the Tauri bridge so that every editing rule can be tested
 * directly, which is exactly the property the Python module was built for.
 */

import type { Element, WindowInfo } from "./bridge";

/** Actions that need a text argument to be meaningful. */
export const ACTIONS_NEEDING_TEXT = ["set_value", "type_text"] as const;

export interface Step {
  id: string;
  action: string;
  /** The `snapshot:revision:node` reference captured when the step was added. */
  target: string;
  /** A human-readable description of the element, for display. */
  summary: string;
  windowId: string;
  windowTitle: string;
  argument?: string;
  enabled: boolean;
}

export interface StepDraft {
  action: string;
  element: Element;
  window: WindowInfo;
  argument?: string;
}

export interface ValidationIssue {
  stepId: string;
  message: string;
}

let counter = 0;

/** Reset the id sequence, for deterministic tests. */
export function resetIds(): void {
  counter = 0;
}

function nextId(): string {
  counter += 1;
  return `step-${counter}`;
}

export class Recording {
  steps: Step[] = [];

  /** The steps that would actually run, in order. */
  get enabledSteps(): Step[] {
    return this.steps.filter((step) => step.enabled);
  }

  add(draft: StepDraft): Step {
    const step: Step = {
      id: nextId(),
      action: draft.action,
      target: draft.element.ref,
      summary: draft.element.summary,
      windowId: draft.window.window_id,
      windowTitle: draft.window.title,
      argument: draft.argument,
      enabled: true,
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
    for (const step of this.enabledSteps) {
      if (
        (ACTIONS_NEEDING_TEXT as readonly string[]).includes(step.action) &&
        !step.argument
      ) {
        issues.push({
          stepId: step.id,
          message: `${step.action} needs text to enter`,
        });
      }
      if (!step.target.includes(":")) {
        issues.push({ stepId: step.id, message: "the step has no usable target" });
      }
    }
    return issues;
  }

  /**
   * Emit a workflow descriptor.
   *
   * The result is the same shape `aad validate` accepts, so what the GUI
   * produces and what the CLI runs cannot drift apart.
   */
  toDescriptor(name = "recorded.workflow"): Record<string, unknown> {
    const steps = this.enabledSteps.map((step) => {
      const args: Record<string, unknown> = { target: step.target };
      if (step.action === "set_value") {
        args.value = step.argument ?? "";
      }
      if (step.action === "type_text") {
        args.text = step.argument ?? "";
      }
      return {
        id: step.id.replace(/-/g, "_"),
        type: "action",
        uses: `desktop.windows_uia.${step.action}@1`,
        with: args,
      };
    });

    return {
      apiVersion: "ai-auto-desktop.dev/v1alpha1",
      kind: "Workflow",
      metadata: { name },
      budgets: {
        max_duration: "5m",
        // Leave headroom so a recording is not rejected the moment it grows.
        max_executed_steps: Math.max(10, steps.length * 2),
      },
      steps,
    };
  }
}
