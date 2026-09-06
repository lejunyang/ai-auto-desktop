/**
 * The typed bridge between the Vue front end and the Rust backend.
 *
 * Everything the UI knows about the desktop arrives through here. Keeping the
 * transport behind one module means the components can be tested without a
 * WebView, and a Tauri command signature change breaks in one place.
 */

export interface Bounds {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface WindowInfo {
  window_id: string;
  title: string;
  process_id: number;
  process_name: string | null;
  class_name: string | null;
  bounds: Bounds | null;
  is_foreground: boolean;
  is_minimized: boolean;
}

/** Which side of the anchor a proximity constraint looks. */
export type LocatorDirection = "any" | "left" | "right" | "above" | "below";

/** Find an element by what it sits next to. */
export interface Proximity {
  /** A full locator: the anchor can itself be positional or nested. */
  anchor: Locator;
  direction?: LocatorDirection;
  /** Largest gap in pixels between the two elements' edges. */
  within?: number;
}

/**
 * A description of an element that stays valid after the snapshot is gone.
 *
 * Attributes identify an element when it has an identity of its own. When it
 * does not -- or when the one it has keeps changing, as a translated label or a
 * per-run id does -- the remaining fields describe where it sits instead: which
 * one of several, and what it is next to.
 */
export interface Locator {
  role?: string;
  name?: string;
  value?: string;
  automation_id?: string;
  class_name?: string;
  framework_id?: string;
  match?: string;
  /** States that must hold, e.g. `{ focusable: true }`. */
  states?: Record<string, boolean>;
  /** 1-based position among the matches, or `"last"`. There is no element zero. */
  nth?: number | string;
  /**
   * Restrict the search to one container's subtree.
   *
   * A full locator, so the container can itself be positional or
   * nested. Distinct from `Proximity.within`, which is a distance in
   * pixels; this one is what holds the element.
   */
  within?: Locator;
  near?: Proximity;
}

/** One element in a window outline, already addressable. */
export interface Element {
  node_id: string;
  /** The `snapshot:revision:node` reference an action must quote right now. */
  ref: string;
  /**
   * How to find this element again later, or null when it cannot be told apart
   * from its siblings. A `ref` dies with its snapshot, so only a locator can be
   * saved to a file and replayed.
   */
  locator: Locator | null;
  depth: number;
  summary: string;
  actions: string[];
  /**
   * The element masks its content, as a password field does.
   *
   * Reported as a flag rather than left to be parsed out of `summary`. The
   * platform withholds such an element's value on its own, so this is what
   * distinguishes "there is nothing here" from "the content is not readable".
   */
  protected?: boolean;
}

export interface Outline {
  snapshot_id: string;
  revision: number;
  window: WindowInfo;
  node_count: number;
  shown: number;
  truncated: boolean;
  elements: Element[];
}

/** A structured backend failure, as opposed to a transport failure. */
export interface DriverFailure {
  code: string;
  message: string;
  retryable: boolean;
  effect: string;
  details?: Record<string, unknown>;
  hint?: string;
}

export class BridgeError extends Error {
  readonly failure: DriverFailure;

  constructor(failure: DriverFailure) {
    super(failure.message);
    this.name = "BridgeError";
    this.failure = failure;
  }

  get code(): string {
    return this.failure.code;
  }

  /** Whether retrying unchanged could plausibly succeed. */
  get retryable(): boolean {
    return this.failure.retryable;
  }
}

type Invoker = (command: string, args?: Record<string, unknown>) => Promise<unknown>;

let invoker: Invoker | null = null;

/** Override the transport, for tests. */
export function setInvoker(next: Invoker | null): void {
  invoker = next;
}

async function resolveInvoker(): Promise<Invoker> {
  if (invoker) {
    return invoker;
  }
  // Imported lazily so the module can be unit tested outside a WebView, where
  // the Tauri API is not injected.
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke as Invoker;
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  const run = await resolveInvoker();
  try {
    return (await run(command, args)) as T;
  } catch (raw) {
    throw new BridgeError(asFailure(raw));
  }
}

/**
 * Normalise whatever the backend rejected with into a usable failure.
 *
 * A thrown string or a stray object must still produce something the UI can
 * render, rather than surfacing `[object Object]` to the user.
 */
export function asFailure(raw: unknown): DriverFailure {
  // A BridgeError already carries a fully-formed failure, so unwrap it before
  // any structural sniffing. Its `code` and `message` are getters rather than
  // own properties, so the checks below would not see them and the hint the
  // backend supplied would be thrown away exactly when it is most needed.
  if (raw instanceof BridgeError) {
    return raw.failure;
  }
  if (typeof raw === "string") {
    try {
      return asFailure(JSON.parse(raw));
    } catch {
      return { code: "GUI.ERROR", message: raw, retryable: false, effect: "unknown" };
    }
  }
  if (raw && typeof raw === "object") {
    const value = raw as Record<string, unknown>;
    if (typeof value.code === "string" && typeof value.message === "string") {
      return {
        code: value.code,
        message: value.message,
        retryable: value.retryable === true,
        effect: typeof value.effect === "string" ? value.effect : "unknown",
        details: value.details as Record<string, unknown> | undefined,
        hint: typeof value.hint === "string" ? value.hint : undefined,
      };
    }
    if (raw instanceof Error) {
      return { code: "GUI.ERROR", message: raw.message, retryable: false, effect: "unknown" };
    }
  }
  return {
    code: "GUI.ERROR",
    message: "The backend failed without a description.",
    retryable: false,
    effect: "unknown",
  };
}

export const bridge = {
  listApps: () => call<{ windows: WindowInfo[]; count: number }>("list_apps"),

  describeWindow: (windowId: string, limit = 120) =>
    call<Outline>("describe_window", { windowId, limit }),

  /** Act on an element, quoting a reference the user actually saw. */
  act: (action: string, target: string, argument?: string) =>
    call<{ applied: boolean; action: string; node_id: string }>("act", {
      action,
      target,
      argument: argument ?? null,
    }),

  /**
   * Begin watching a window, returning a session id and which mechanisms
   * actually attached.
   *
   * `sources` matters to the user, not just to a log: WinForms controls are only
   * seen by the WinEvent hook and Chromium's are only named by the UIA handler,
   * so a session with one source silently misses a whole class of interaction.
   */
  startRecording: (windowId: string) =>
    call<CaptureSession>("start_recording", { windowId }),

  /** Take the steps observed since the last call. */
  collectRecording: (captureId: string) =>
    call<CaptureBatch>("collect_recording", { captureId }),

  stopRecording: (captureId: string) =>
    call<{ released: boolean }>("stop_recording", { captureId }),

  /**
   * Try a locator against a live window and report what it selects.
   *
   * Rejects with DRIVER.AMBIGUOUS_MATCH when several elements match, and the
   * candidates in its details are the point: they are what tells someone how to
   * narrow the locator. A miss comes back as `found: false` rather than an
   * error, because a locator being edited matches nothing most of the way.
   */
  tryLocator: (windowId: string, locator: Locator) =>
    call<{
      found: boolean;
      node?: Record<string, unknown>;
      match_count?: number;
    }>("try_locator", { windowId, locator }),

  probe: () => call<Record<string, unknown>>("probe_environment"),

  /**
   * Save the editable recording and its runnable workflow together.
   *
   * Both in one call so the two files cannot disagree about what was recorded.
   */
  saveRecording: (name: string, document: unknown, workflow: unknown) =>
    call<{ recording_path: string; workflow_path: string }>("save_recording", {
      name,
      document,
      workflow,
    }),

  /**
   * Replay a workflow and report how every step went.
   *
   * The per-step outcomes are the point, not the overall status: a replay that
   * finds the wrong element still reports success at the run level, and catching
   * exactly that is why one would replay inside the editor. A run that stops
   * partway says which step failed and how many ran, which is what decides
   * whether trying again is safe -- a step that wrote a value but never submitted
   * it will write it twice.
   */
  runWorkflow: (workflow: unknown, inputs?: Record<string, unknown>) =>
    call<RunOutcome>("run_workflow", { workflow, inputs: inputs ?? null }),

  loadRecording: (path: string) => call<unknown>("load_recording", { path }),

  listRecordings: () =>
    call<{ recordings: SavedRecording[]; directory: string }>("list_recordings"),
};

/** One saved recording, as offered in the open list. */
/** How one step of a replay went. */
export interface StepOutcome {
  id: string;
  status: "succeeded" | "failed" | string;
  error?: { code?: string; message?: string } | null;
}

/** What came back from replaying a workflow. */
export interface RunOutcome {
  run_id: string;
  workflow: string;
  status: "succeeded" | "failed" | string;
  executed_steps: number;
  duration_seconds: number;
  steps: StepOutcome[];
  error?: {
    code: string;
    message: string;
    retryable?: boolean;
    /** Whether the desktop was already changed; the one thing a reader cannot re-derive. */
    effect?: string;
  } | null;
}

export interface SavedRecording {
  name: string;
  path: string;
  modified: number | null;
}

/** A live capture session. */
export interface CaptureSession {
  capture_id: string;
  window_id: string;
  /** Which capture mechanisms attached, e.g. `uia`, `win_event`. */
  sources: string[];
  baseline_nodes: number;
  snapshot_id: string;
}

/** One batch of steps taken from a running session. */
export interface CaptureBatch {
  capture_id: string;
  window_id: string;
  steps: CapturedStepPayload[];
  count: number;
  /**
   * Events the bounded buffer had to discard.
   *
   * Surfaced rather than swallowed: a dropped event is indistinguishable from
   * the user not having done anything, so a recording missing a step would look
   * like a recording of fewer actions.
   */
  dropped: number;
  raw_events: number;
}

/** A step as the backend describes it, before the UI adopts it. */
export interface CapturedStepPayload {
  action: string;
  locator: Locator | null;
  summary: string;
  argument: string | null;
  protected: boolean;
  replayable: boolean;
  unresolved: string | null;
}
