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
  /** `focusable`, `enabled` and so on, as a state that must be true. */
  requireState: string;
  /** 1-based position among the matches, or empty for none. Also accepts `last`. */
  nth: string;
  /** The name of the element this one sits beside. */
  nearName: string;
  nearRole: string;
  direction: Direction;
  /** Maximum gap in pixels, or empty for no limit. */
  within: string;
}

export function emptyDraft(): LocatorDraft {
  return {
    role: "",
    name: "",
    automationId: "",
    className: "",
    requireState: "",
    nth: "",
    nearName: "",
    nearRole: "",
    direction: "any",
    within: "",
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
      draft.within = String(proximity.within);
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
    "states",
    "nth",
    "near",
  ]);
  if (Object.keys(source).some((key) => !known.has(key))) {
    return true;
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
    draft.role || draft.name || draft.automationId || draft.className || draft.requireState;
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
  if (draft.within && !draft.nearName) {
    return "a distance limit only means something with an element to be near";
  }
  if (draft.within) {
    const within = Number(draft.within);
    if (!Number.isFinite(within) || within <= 0) {
      return "a distance is a number of pixels";
    }
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
    if (draft.within.trim()) {
      near.within = Number(draft.within.trim());
    }
    locator.near = near;
  }
  return locator as Locator;
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
  return parts.join(" ");
}

function asText(value: unknown): string {
  return typeof value === "string" ? value : "";
}
