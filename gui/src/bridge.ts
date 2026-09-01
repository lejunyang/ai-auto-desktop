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

/** One element in a window outline, already addressable. */
export interface Element {
  node_id: string;
  /** The `snapshot:revision:node` reference an action must quote. */
  ref: string;
  depth: number;
  summary: string;
  actions: string[];
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

  probe: () => call<Record<string, unknown>>("probe_environment"),
};
