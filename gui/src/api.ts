/**
 * The typed calls into the Rust backend, and the shapes they exchange.
 *
 * Every shape mirrors `aad_control::steps` and the control session, so what
 * the editor saves is exactly what `aad run` replays on any platform.
 */
import { invoke } from "@tauri-apps/api/core";

/** Which application or page a step works in; every given field must match. */
export interface App {
  provider?: string;
  process?: string;
  title?: string;
  url?: string;
}

/** How to find an element again. role and name work on every platform. */
export interface Locator {
  role?: string;
  name?: string;
  node_id?: string;
  value?: string;
  automation_id?: string;
  class_name?: string;
  framework_id?: string;
  states?: Record<string, boolean>;
  match?: "exact" | "contains";
  within?: Locator;
  near?: { anchor: Locator; direction?: string; within?: number };
  nth?: number | "first" | "last";
}

export interface Fraction {
  fx: number;
  fy: number;
}

/** A control action as saved: a drag's destination lives on the step. */
export type Action =
  | { type: "focus" | "invoke" | "back" | "forward" | "reload" }
  | { type: "click"; button?: "left" | "right" | "middle"; count?: 1 | 2; fraction?: Fraction }
  | { type: "fill"; value?: string }
  | { type: "type_text"; text?: string; mode?: "unicode" | "keys" }
  | { type: "key"; keys: string[] }
  | { type: "scroll"; direction: "up" | "down" | "left" | "right"; notches?: number }
  | { type: "hover"; ms?: number }
  | { type: "drag"; button?: "left" | "right" | "middle" }
  | { type: "select"; option: string }
  | { type: "check"; checked: boolean }
  | { type: "navigate"; url: string };

export type ActionType = Action["type"];

export interface Step {
  id: string;
  app: App;
  locator: Locator;
  action?: Action;
  to?: Locator;
  check?: { present: boolean; within_ms: number };
  input?: { name: string; sensitive: boolean };
  note?: string;
}

export interface Resource {
  resource_id: string;
  provider: string;
  kind: "desktop" | "browser" | string;
  name: string;
  capabilities: string[];
  metadata: Record<string, unknown>;
  /** The name a saved step would give this window. */
  app: App;
}

export interface Targets {
  resources: Resource[];
  unavailable: Record<string, { code?: string; message?: string }>;
  recording_supported: boolean;
  browser_port: number | null;
}

export interface ElementNode {
  node_id: string;
  role: string;
  name: string;
  value?: string | null;
  capabilities: string[];
  options?: string[];
  states?: Record<string, unknown>;
  /** Where the element sits in the tree, for indenting. */
  depth?: number;
  /** The locator a step would save for this element. */
  step_locator: Locator;
}

export interface Region {
  name: string;
  role?: string;
  elements?: number;
  [key: string]: unknown;
}

export interface Observation {
  nodes?: ElementNode[];
  regions?: Region[];
  matched?: number;
  shown?: number;
  stopped_by?: string | null;
  node_count: number;
  app: App;
}

export interface Failure {
  code: string;
  message: string;
  effect?: string;
  hint?: string;
  details?: Record<string, unknown>;
}

export interface Found {
  found: boolean;
  match_count: number;
  node?: ElementNode;
}

export interface StepOutcome {
  id: string;
  status: string;
  error?: Failure | null;
}

export interface RunResult {
  status: string;
  steps: StepOutcome[];
  error?: Failure | null;
  duration_seconds?: number;
}

export interface Batch {
  steps: Step[];
  skipped: { reason: string; summary?: string }[];
  dropped?: number;
  recording: boolean;
}

export interface SavedRecording {
  name: string;
  path: string;
  modified?: string | null;
}

/** Anything thrown by a call, as the structured failure the UI shows. */
export function asFailure(error: unknown): Failure {
  if (error && typeof error === "object" && "code" in error) return error as Failure;
  return { code: "GUI.UNEXPECTED", message: String(error) };
}

type Caller = <T>(command: string, args?: Record<string, unknown>) => Promise<T>;
let call: Caller = (command, args) => invoke(command, args);

/** Replace the transport, for tests. */
export function useTransport(transport: Caller): void {
  call = transport;
}

export const api = {
  targets: () => call<Targets>("list_targets"),
  observe: (resourceId: string, view: Record<string, unknown>) =>
    call<Observation>("observe", { resourceId, view }),
  tryLocator: (resourceId: string, locator: Locator) =>
    call<Found>("try_locator", { resourceId, locator }),
  perform: (step: Step, inputs: Record<string, string>) =>
    call<Record<string, unknown>>("perform_step", { step, inputs }),
  run: (name: string, steps: Step[], inputs: Record<string, string>) =>
    call<RunResult>("run_steps", { name, steps, inputs }),
  startRecording: (resourceId: string) => call<unknown>("start_recording", { resourceId }),
  collect: () => call<Batch>("collect_recording"),
  stopRecording: () => call<Batch>("stop_recording"),
  connectBrowser: (port: number | null) => call<Targets>("connect_browser", { port }),
  save: (name: string, steps: Step[]) =>
    call<{ recording_path: string; workflow_path: string }>("save_recording", { name, steps }),
  open: (path: string) => call<{ name: string; steps: Step[] }>("open_recording", { path }),
  saved: () => call<{ recordings: SavedRecording[]; directory: string }>("list_recordings"),
};
