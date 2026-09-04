import { describe as group, expect, it } from "vitest";
import {
  describe as describeLocator,
  draftProblem,
  emptyDraft,
  fromDraft,
  isBeyondForm,
  toDraft,
  type LocatorDraft,
} from "./locator";
import type { Locator } from "./bridge";

/** A draft with only the named fields set. */
function draft(over: Partial<LocatorDraft> = {}): LocatorDraft {
  return { ...emptyDraft(), ...over };
}

group("editing a locator through the form", () => {
  it("builds the four descriptions this is for", () => {
    // The shapes asked for: the first focusable input, the third button, the
    // element beside a label, and a button by its text.
    expect(fromDraft(draft({ role: "edit", requireState: "focusable", nth: "1" }))).toEqual({
      role: "edit",
      states: { focusable: true },
      nth: 1,
    });
    expect(fromDraft(draft({ role: "button", nth: "3" }))).toEqual({
      role: "button",
      nth: 3,
    });
    expect(
      fromDraft(draft({ role: "edit", nearName: "Name:", direction: "right" })),
    ).toEqual({
      role: "edit",
      near: { anchor: { name: "Name:" }, direction: "right" },
    });
    expect(fromDraft(draft({ role: "button", name: "Submit" }))).toEqual({
      role: "button",
      name: "Submit",
    });
  });

  it("survives a round trip without losing a constraint", () => {
    // Losing one silently is the failure that matters: the step still looks
    // edited and now selects something else.
    const original: Locator = {
      role: "button",
      name: "Open",
      automation_id: "openButton",
      class_name: "Btn",
      states: { focusable: true },
      nth: 2,
      near: { anchor: { name: "Files", role: "text" }, direction: "below", within: 60 },
    } as Locator;

    expect(fromDraft(toDraft(original))).toEqual(original);
  });

  it("keeps `last` as a word rather than turning it into a number", () => {
    // Number("last") is NaN, which would serialise as null and match nothing.
    expect(fromDraft(draft({ role: "button", nth: "last" })).nth).toBe("last");
    expect(fromDraft(draft({ role: "button", nth: "3" })).nth).toBe(3);
  });

  it("refuses position zero", () => {
    // The driver rejects it outright. Reading it as "the first" would quietly
    // select a different element than the person meant.
    expect(draftProblem(draft({ role: "button", nth: "0" }))).toContain("no element zero");
    expect(draftProblem(draft({ role: "button", nth: "1" }))).toBeNull();
  });

  it("refuses a locator that constrains nothing", () => {
    // It would match every element in the window.
    expect(draftProblem(emptyDraft())).toContain("constrain something");
  });

  it("refuses a direction or distance with nothing to be near", () => {
    // Both are properties of a proximity constraint, so without an anchor they
    // would be dropped on the way to the driver -- and the person would think
    // they had applied them.
    expect(draftProblem(draft({ role: "button", direction: "left" }))).toContain(
      "element to be near",
    );
    expect(draftProblem(draft({ role: "button", within: "40" }))).toContain(
      "element to be near",
    );
  });

  it("accepts a position on its own", () => {
    // "the third one" is a complete thought when the outline is in front of you.
    expect(draftProblem(draft({ nth: "3" }))).toBeNull();
  });
});

group("locators the form cannot show", () => {
  it("reports a nested anchor rather than flattening it", () => {
    // The form's anchor is a name and a role. A locator whose anchor is itself
    // positional says more than that, and editing through the form would drop
    // the part it cannot see.
    const nested = {
      role: "button",
      near: { anchor: { role: "edit", nth: 2 }, direction: "right" },
    } as Locator;

    expect(isBeyondForm(nested)).toBe(true);
  });

  it("reports a state that must be false", () => {
    // The form offers "this state must be true"; the opposite is a real
    // constraint it cannot express.
    expect(isBeyondForm({ role: "button", states: { enabled: false } } as Locator)).toBe(
      true,
    );
  });

  it("reports several states at once", () => {
    expect(
      isBeyondForm({ states: { focusable: true, enabled: true } } as Locator),
    ).toBe(true);
  });

  it("reports an unknown field instead of discarding it", () => {
    // A locator using something this version of the form does not know about
    // must not come back without it.
    expect(isBeyondForm({ role: "button", match_mode: "substring" } as Locator)).toBe(true);
  });

  it("accepts the shapes it can show", () => {
    expect(isBeyondForm({ role: "edit", name: "NameBox" } as Locator)).toBe(false);
    expect(isBeyondForm({ role: "button", nth: 3 } as Locator)).toBe(false);
    expect(
      isBeyondForm({
        role: "edit",
        states: { focusable: true },
        near: { anchor: { name: "Name:", role: "text" }, direction: "right", within: 40 },
      } as Locator),
    ).toBe(false);
  });
});

group("describing a locator in words", () => {
  it("reads as a description rather than a field dump", () => {
    expect(
      describeLocator({
        role: "edit",
        states: { focusable: true },
        nth: 1,
      } as Locator),
    ).toBe("#1 focusable edit");
    expect(
      describeLocator({
        role: "button",
        near: { anchor: { name: "Name:" }, direction: "right", within: 40 },
      } as Locator),
    ).toBe('button right of "Name:" within 40px');
  });

  it("says plainly when there is no locator at all", () => {
    // This is the state a person has to act on, so it cannot read as empty.
    expect(describeLocator(null)).toContain("could not be identified");
  });
});
