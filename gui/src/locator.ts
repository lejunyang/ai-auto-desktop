/**
 * Reading and writing locators in the forms a person edits them in.
 *
 * A recorded locator names attributes: `{"role": "edit", "name": "NameBox"}`.
 * When that fails to match -- the label was translated, the field was renamed,
 * a second button appeared -- the fix is often not another attribute but a
 * description of where the element sits: the third button, the field beside
 * "Name:", the first focusable input.
 *
 * The driver already understands those. This module is what lets someone write
 * one without hand-editing JSON, where a mistyped direction or `nth: 0` is only
 * discovered when the locator is tried.
 */

import type { Locator } from "./bridge";

/** The directions the driver accepts for a proximity constraint. */
export const DIRECTIONS = ["any", "left", "right", "above", "below"] as const;
export type Direction = (typeof DIRECTIONS)[number];

/**
 * A locator as the editor's fields see it.
 *
 * Flat rather than nested, because that is what a form can bind to. The anchor
 * is only its name and role: an anchor is itself a full locator and can nest, but
 * a form cannot express that, and the JSON view exists for the cases that need
 * it.
 */
export interface LocatorDraft {
  role: string;
  name: string;
  automationId: string;
  className: string;
  /**
   * The UI toolkit, e.g. `WinForm` or `WPF`.
   *
   * Included because synthesis uses it and the driver always reports it. Left
   * out, a locator carrying one was judged beyond the form -- so the editor
   * stayed in the JSON view and the "use fields" button did nothing, which
   * looks like a broken button rather than a missing field.
   */
  frameworkId: string;
  /** `focusable`, `enabled` and so on, as a state that must be true. */
  requireState: string;
  /** 1-based position among the matches, or empty for none. Also accepts `last`. */
  nth: string;
  /** The name of the element this one sits beside. */
  nearName: string;
  nearRole: string;
  direction: Direction;
  /**
   * Maximum gap in pixels between this element and its anchor, or empty for no
   * limit.
   *
   * Named apart from `containerName` on purpose: the driver calls both of them
   * `within` -- a number under `near`, an object at the top level -- and a form
   * cannot bind two fields to one name. Sharing it would send "40" and
   * "tool_bar" to the same place.
   */
  nearWithin: string;
  /**
   * The name of the container to search inside, or empty to search the window.
   *
   * The reason this exists: of the interactive elements measured on this machine
   * that no attribute combination could identify, naming a container brought
   * same-role siblings from a median of 66 down to 3.
   */
  containerName: string;
  containerRole: string;
  /**
   * The container's automation id, when that is what identifies it.
   *
   * Without this the whole locator had to stay in the JSON view: measured on the
   * fixture page, `billing-panel` and `delivery-panel` are two structurally
   * identical panels whose only distinguishing field is the id.
   */
  containerId: string;
  /**
   * The container's own container.
   *
   * Synthesis nests one level when the immediate container is anonymous, and the
   * outer level is often the part that distinguishes: a table button comes back
   * as the cell (four identical siblings across the table) inside the row
   * ("Order for Ada"). Two levels is what synthesis produces; deeper stays in
   * the JSON view rather than becoming a form nobody can read.
   */
  outerName: string;
  outerRole: string;
  outerId: string;
}

export function emptyDraft(): LocatorDraft {
  return {
    role: "",
    name: "",
    automationId: "",
    className: "",
    frameworkId: "",
    requireState: "",
    nth: "",
    nearName: "",
    nearRole: "",
    direction: "any",
    nearWithin: "",
    containerName: "",
    containerRole: "",
    containerId: "",
    outerName: "",
    outerRole: "",
    outerId: "",
  };
}

/** The states worth offering, being the ones an element actually reports. */
export const REQUIRABLE_STATES = ["focusable", "enabled", "focused", "read_only"] as const;

/**
 * Fill the editor's fields from a locator.
 *
 * Anything the form cannot represent is left out of the draft rather than
 * mangled into it, and `isBeyondForm` reports that so the editor can keep the
 * user in the JSON view instead of silently dropping half their locator.
 */
export function toDraft(locator: Locator | null): LocatorDraft {
  const draft = emptyDraft();
  if (!locator) {
    return draft;
  }
  const source = locator as Record<string, unknown>;
  draft.role = asText(source.role);
  draft.name = asText(source.name);
  draft.automationId = asText(source.automation_id);
  draft.className = asText(source.class_name);
  draft.frameworkId = asText(source.framework_id);

  const states = source.states;
  if (states && typeof states === "object") {
    // Only a single true state, which is what the form offers. A locator
    // requiring several is beyond the form and reported as such.
    const entries = Object.entries(states as Record<string, unknown>).filter(
      ([, value]) => value === true,
    );
    if (entries.length === 1) {
      draft.requireState = entries[0][0];
    }
  }

  if (source.nth !== undefined && source.nth !== null) {
    draft.nth = String(source.nth);
  }

  const near = source.near;
  if (near && typeof near === "object") {
    const proximity = near as Record<string, unknown>;
    const anchor = proximity.anchor;
    if (anchor && typeof anchor === "object") {
      const fields = anchor as Record<string, unknown>;
      draft.nearName = asText(fields.name);
      draft.nearRole = asText(fields.role);
    }
    const direction = asText(proximity.direction);
    if ((DIRECTIONS as readonly string[]).includes(direction)) {
      draft.direction = direction as Direction;
    }
    if (proximity.within !== undefined && proximity.within !== null) {
      draft.nearWithin = String(proximity.within);
    }
  }

  const container = source.within;
  if (container && typeof container === "object") {
    const fields = container as Record<string, unknown>;
    draft.containerName = asText(fields.name);
    draft.containerRole = asText(fields.role);
    draft.containerId = asText(fields.automation_id);
    const outer = fields.within;
    if (outer && typeof outer === "object") {
      const outerFields = outer as Record<string, unknown>;
      draft.outerName = asText(outerFields.name);
      draft.outerRole = asText(outerFields.role);
      draft.outerId = asText(outerFields.automation_id);
    }
  }
  return draft;
}

/**
 * Whether this locator says more than the form can show.
 *
 * Editing through the form would then lose part of it, so the editor has to keep
 * such a locator in the JSON view. Silently round-tripping it would produce a
 * locator that matches something else while looking edited.
 */
export function isBeyondForm(locator: Locator | null): boolean {
  if (!locator) {
    return false;
  }
  const source = locator as Record<string, unknown>;
  const known = new Set([
    "role",
    "name",
    "automation_id",
    "class_name",
    "framework_id",
    "states",
    "nth",
    "near",
    "within",
  ]);
  if (Object.keys(source).some((key) => !known.has(key))) {
    return true;
  }
  // The form now holds two levels of container, each identified by name, role or
  // id -- which is everything synthesis produces. What it still cannot hold is a
  // third level, or a container narrowed by a position or a state.
  const containerFields = new Set(["name", "role", "automation_id"]);
  let container = source.within;
  let level = 0;
  while (container && typeof container === "object") {
    level += 1;
    if (level > 2) {
      return true;
    }
    const fields = container as Record<string, unknown>;
    if (Object.keys(fields).some((key) => key !== "within" && !containerFields.has(key))) {
      return true;
    }
    container = fields.within;
  }
  const states = source.states;
  if (states && typeof states === "object") {
    const entries = Object.entries(states as Record<string, unknown>);
    // More than one state, or a state required to be false: both real and both
    // outside what the form offers.
    if (entries.length > 1 || entries.some(([, value]) => value !== true)) {
      return true;
    }
  }
  const near = source.near;
  if (near && typeof near === "object") {
    const anchor = (near as Record<string, unknown>).anchor;
    if (anchor && typeof anchor === "object") {
      const fields = Object.keys(anchor as Record<string, unknown>);
      // A nested anchor, or one identified by anything other than name/role.
      if (fields.some((key) => key !== "name" && key !== "role")) {
        return true;
      }
    }
  }
  return false;
}

/** What is wrong with a draft, or null when it is usable. */
export function draftProblem(draft: LocatorDraft): string | null {
  const constrains =
    draft.role ||
    draft.name ||
    draft.automationId ||
    draft.className ||
    draft.frameworkId ||
    draft.requireState;
  if (!constrains && !draft.nth && !draft.nearName) {
    return "a locator has to constrain something";
  }
  if (draft.nth) {
    const nth = draft.nth.trim().toLowerCase();
    if (nth !== "last" && nth !== "first") {
      const parsed = Number(nth);
      if (!Number.isInteger(parsed) || parsed < 1) {
        // The driver refuses 0 outright: positions are 1-based, and reading 0 as
        // "the first" would quietly select a different element than intended.
        return "a position is 1, 2, 3… or `last` — there is no element zero";
      }
    }
  }
  if (draft.nearWithin && !draft.nearName) {
    return "a distance limit only means something with an element to be near";
  }
  if (draft.nearWithin) {
    const within = Number(draft.nearWithin);
    if (!Number.isFinite(within) || within <= 0) {
      return "a distance is a number of pixels";
    }
  }
  if (draft.containerRole.trim() && !draft.containerName.trim()) {
    // A role alone usually matches several containers, and the driver treats an
    // ambiguous container as no match -- so this would look like a working
    // locator that finds nothing. The JSON view can still express it for the
    // cases where the role really is unique.
    return "a container needs a name — a role alone usually matches several";
  }
  if (draft.direction !== "any" && !draft.nearName) {
    return "a direction only means something with an element to be near";
  }
  return null;
}

/** Build the locator a draft describes. */
export function fromDraft(draft: LocatorDraft): Locator {
  const locator: Record<string, unknown> = {};
  if (draft.role.trim()) {
    locator.role = draft.role.trim();
  }
  if (draft.name.trim()) {
    locator.name = draft.name.trim();
  }
  if (draft.automationId.trim()) {
    locator.automation_id = draft.automationId.trim();
  }
  if (draft.className.trim()) {
    locator.class_name = draft.className.trim();
  }
  if (draft.frameworkId.trim()) {
    locator.framework_id = draft.frameworkId.trim();
  }
  if (draft.requireState) {
    locator.states = { [draft.requireState]: true };
  }
  if (draft.nth.trim()) {
    const nth = draft.nth.trim().toLowerCase();
    locator.nth = nth === "last" || nth === "first" ? nth : Number(nth);
  }
  if (draft.nearName.trim()) {
    const anchor: Record<string, unknown> = { name: draft.nearName.trim() };
    if (draft.nearRole.trim()) {
      anchor.role = draft.nearRole.trim();
    }
    const near: Record<string, unknown> = { anchor };
    if (draft.direction !== "any") {
      near.direction = draft.direction;
    }
    if (draft.nearWithin.trim()) {
      near.within = Number(draft.nearWithin.trim());
    }
    locator.near = near;
  }
  // Built inner-first so the outer level ends up wrapping the inner one, which
  // is the order the driver walks: contain, then count.
  const container: Record<string, unknown> = {};
  if (draft.containerName.trim()) {
    container.name = draft.containerName.trim();
  }
  if (draft.containerRole.trim()) {
    container.role = draft.containerRole.trim();
  }
  if (draft.containerId.trim()) {
    container.automation_id = draft.containerId.trim();
  }
  const outer: Record<string, unknown> = {};
  if (draft.outerName.trim()) {
    outer.name = draft.outerName.trim();
  }
  if (draft.outerRole.trim()) {
    outer.role = draft.outerRole.trim();
  }
  if (draft.outerId.trim()) {
    outer.automation_id = draft.outerId.trim();
  }
  if (Object.keys(outer).length) {
    // An outer container with no inner one means the user filled the wrong box;
    // treating it as the container is what they meant.
    if (Object.keys(container).length) {
      container.within = outer;
    } else {
      Object.assign(container, outer);
    }
  }
  if (Object.keys(container).length) {
    locator.within = container;
  }
  return locator as Locator;
}

/**
 * Whether a locator counts without saying where to count.
 *
 * A position on its own is measured across every element in the window, the
 * frame included -- so the third button on a plain window is its close button,
 * and in a browser the first five are all toolbar (measured: 20 buttons reported
 * for a page containing three). An anchor makes the count local, so only a
 * position without one is worth warning about.
 *
 * Not an error: counting across a whole window is sometimes exactly right, and
 * refusing it would block a locator that works.
 */
export function countsAcrossWindow(locator: Locator | null): boolean {
  if (!locator) {
    return false;
  }
  const source = locator as Record<string, unknown>;
  if (source.nth === undefined || source.nth === null) {
    return false;
  }
  // Either kind of scope makes the count local: an anchor restricts it to what
  // sits nearby, a container to one subtree. Warning about a scoped position
  // would train people to ignore the warning.
  return source.near === undefined && source.within === undefined;
}

/** A one-line description of what a locator selects, for display. */
export function describe(locator: Locator | null): string {
  if (!locator) {
    return "nothing — this element could not be identified";
  }
  const source = locator as Record<string, unknown>;
  const parts: string[] = [];

  const nth = source.nth;
  if (nth !== undefined && nth !== null) {
    parts.push(nth === "last" ? "the last" : `#${nth}`);
  }

  const states = source.states;
  if (states && typeof states === "object") {
    const flags = Object.entries(states as Record<string, unknown>)
      .filter(([, value]) => value === true)
      .map(([key]) => key);
    if (flags.length) {
      parts.push(flags.join(" and "));
    }
  }

  parts.push(asText(source.role) || "element");
  const name = asText(source.name);
  if (name) {
    parts.push(`named ${JSON.stringify(name)}`);
  }
  const automationId = asText(source.automation_id);
  if (automationId) {
    parts.push(`id ${JSON.stringify(automationId)}`);
  }

  const near = source.near;
  if (near && typeof near === "object") {
    const proximity = near as Record<string, unknown>;
    const anchor = (proximity.anchor ?? {}) as Record<string, unknown>;
    const anchorName = asText(anchor.name) || asText(anchor.role) || "something";
    const direction = asText(proximity.direction);
    const where =
      direction && direction !== "any" ? `${direction} of` : "beside";
    parts.push(`${where} ${JSON.stringify(anchorName)}`);
    if (proximity.within !== undefined && proximity.within !== null) {
      parts.push(`within ${proximity.within}px`);
    }
  }

  // Walked to the bottom rather than one level deep. Synthesis nests containers
  // when the immediate one is anonymous, and the distinguishing part is usually
  // the innermost: for a table row the outer level is the cell (four identical
  // siblings across the table) and the inner one names the customer. Reporting
  // only the outer level describes a locator that would be ambiguous, which is
  // worse than a long description -- this is what a person checks the locator
  // against.
  let container = source.within;
  let depth = 0;
  while (container && typeof container === "object" && depth < 4) {
    parts.push(`inside ${containerLabel(container as Record<string, unknown>)}`);
    container = (container as Record<string, unknown>).within;
    depth += 1;
  }
  return parts.join(" ");
}

/**
 * How to refer to a container, using whichever field actually identifies it.
 *
 * Falling back to the role alone reported `billing-panel` as `"group"`, which
 * names the wrong thing entirely: every panel on the page is a group.
 */
function containerLabel(fields: Record<string, unknown>): string {
  const name = asText(fields.name);
  if (name) {
    return JSON.stringify(name);
  }
  const automationId = asText(fields.automation_id);
  if (automationId) {
    return `id ${JSON.stringify(automationId)}`;
  }
  const role = asText(fields.role);
  const ordinal = fields.nth;
  if (role && ordinal !== undefined && ordinal !== null) {
    return ordinal === "last" ? `the last ${role}` : `${role} #${ordinal}`;
  }
  return role ? `the ${role}` : "an unnamed container";
}

function asText(value: unknown): string {
  return typeof value === "string" ? value : "";
}
