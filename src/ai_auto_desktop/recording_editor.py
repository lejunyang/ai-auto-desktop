"""Editing operations for a recording, with validation before commit.

Why a module rather than logic inside the HTTP layer: every operation here has
to be testable without a browser, a socket or a port, and the transport must not
be able to reach recording internals.  The server calls into this; it does not
reimplement any of it.

Two measured facts shape the design.

`compile_recording()` already enforces reference integrity, disabled-step
references and input declarations, and it leaves the recording untouched when it
raises.  So editing does not reimplement validation -- it applies the change to a
copy, asks the compiler, and keeps the copy only if the compiler accepts.  A
rejected edit therefore cannot leave a half-applied recording behind.

Reordering is different.  Measured: moving a logic step ahead of the interaction
that opens its dialog compiles cleanly, because no expression references it.  No
static check can catch that, so reordering reports warnings and still applies;
refusing would block a deliberate restructure, and silence would hide a break.
"""

from __future__ import annotations

import copy
import json
import re
from collections.abc import Iterable, Mapping, MutableMapping, Sequence
from typing import Any

from .recording import RecordingError, compile_recording

RECORDED_ORDER_KEY = "ai-auto-desktop.dev/recorded-order"

# Fields an operator may change on a step.  Deliberately not a free-form merge:
# an editor that can write any key can also write `kind`, `of_step` or
# `disambiguation`, which would let the UI forge a verification it never did.
EDITABLE_FIELDS = frozenset({"description", "enabled", "locator", "window",
                             "text", "logic"})

# Fields that record what was observed at capture time.  Editing them would make
# the artifact lie about the recording session.
PROTECTED_FIELDS = frozenset({"id", "kind", "action", "of_step", "observed",
                              "disambiguation", "sequence"})


class EditError(RecordingError):
    """An edit that must be refused, with the reason the operator needs."""


def _steps_index(steps: Sequence[Mapping[str, Any]]) -> dict[str, Mapping[str, Any]]:
    found: dict[str, Mapping[str, Any]] = {}
    for step in steps:
        found[step["id"]] = step
        if step.get("kind") == "logic":
            found.update(_steps_index(step.get("steps", [])))
    return found


def _locate(steps: list[MutableMapping[str, Any]], step_id: str
            ) -> tuple[list[MutableMapping[str, Any]], int] | None:
    """Find the list containing ``step_id`` and its index within it.

    Returns the container so that nested logic children can be edited and
    reordered in place, rather than only top-level steps.
    """

    for index, step in enumerate(steps):
        if step["id"] == step_id:
            return steps, index
        if step.get("kind") == "logic":
            nested = _locate(step.setdefault("steps", []), step_id)
            if nested is not None:
                return nested
    return None


def _validated(recording: Mapping[str, Any]) -> dict[str, Any]:
    """Return the recording if it is structurally sound enough to keep editing.

    Deliberately weaker than ``compile_recording``.  Editing a locator clears
    its verification, which the compiler rejects -- so validating edits with the
    compiler would make "fix a locator, then re-verify it" impossible: the fix
    could never be saved.  Saveable and compilable are different questions, and
    ``compile_preview`` answers the second.

    What is still enforced is what would corrupt the artifact rather than merely
    leave it unfinished: duplicate ids, references to steps that do not exist,
    and assertions attached to nothing.
    """

    candidate = copy.deepcopy(dict(recording))
    steps = candidate.get("steps", [])
    if not isinstance(steps, list):
        raise EditError("RECORDING.INVALID", "steps must be a list")

    seen: set[str] = set()

    def check(nodes: Sequence[Mapping[str, Any]]) -> None:
        for node in nodes:
            step_id = node.get("id")
            if not isinstance(step_id, str) or not step_id:
                raise EditError("RECORDING.INVALID", "step needs an id")
            if step_id in seen:
                # Two steps with one id make every reference ambiguous, and the
                # UI could no longer address them separately.
                raise EditError("RECORDING.ORDER_INVALID",
                                "duplicate step id", id=step_id)
            seen.add(step_id)
            if node.get("kind") == "logic":
                check(node.get("steps", []))

    check(steps)

    known = set(_steps_index(steps))
    for node in _flatten(steps):
        if node.get("kind") != "assertion":
            continue
        target = node.get("of_step")
        if target not in known:
            # A dangling assertion is the one case the workflow compiler was
            # measured NOT to catch, so the editor has to.
            raise EditError("RECORDING.ORDER_INVALID",
                            "assertion refers to a step that does not exist",
                            id=node.get("id"), of_step=target)
    return candidate


def _flatten(steps: Sequence[Mapping[str, Any]]) -> list[Mapping[str, Any]]:
    found: list[Mapping[str, Any]] = []
    for step in steps:
        found.append(step)
        if step.get("kind") == "logic":
            found.extend(_flatten(step.get("steps", [])))
    return found


def _recorded_order(recording: Mapping[str, Any]) -> list[str]:
    annotations = (recording.get("metadata") or {}).get("annotations") or {}
    raw = annotations.get(RECORDED_ORDER_KEY, "")
    return [part for part in raw.split(",") if part]


def order_warnings(recording: Mapping[str, Any]) -> list[dict[str, Any]]:
    """Report ways the current order may break replay, without blocking.

    Measured: a reorder that moves a step ahead of the interaction which
    established its precondition compiles cleanly, because nothing references it
    in an expression.  Static analysis cannot detect that, so the operator is
    told and left to decide.
    """

    warnings: list[dict[str, Any]] = []
    steps = recording.get("steps", [])
    current = [step["id"] for step in steps]
    recorded = [step_id for step_id in _recorded_order(recording)
                if step_id in set(current)]

    if recorded and current != recorded:
        warnings.append({
            "code": "ORDER_DIVERGED",
            "message": "step order differs from the recorded order",
            "recorded": recorded,
            "current": current,
        })

    # An interaction that moved ahead of an assertion which was recorded before
    # it is the concrete shape of the precondition problem above.
    position = {step_id: index for index, step_id in enumerate(current)}
    for step in steps:
        if step.get("kind") != "assertion":
            continue
        target = step.get("of_step")
        if target in position and position[target] < position[step["id"]]:
            continue
        if target in position:
            warnings.append({
                "code": "ASSERTION_BEFORE_TARGET",
                "message": "assertion is ordered before the step it checks",
                "id": step["id"],
                "of_step": target,
            })
    return warnings


def set_enabled(recording: Mapping[str, Any], step_id: str, enabled: bool
                ) -> dict[str, Any]:
    """Enable or disable a step.

    Disabling is offered instead of deletion because it cannot create a dangling
    reference: the step stays in the artifact, so anything pointing at it still
    resolves, and the compiler reports the disabled reference explicitly rather
    than silently dropping a dependency.
    """

    draft = copy.deepcopy(dict(recording))
    found = _locate(draft.setdefault("steps", []), step_id)
    if found is None:
        raise EditError("RECORDING.ORDER_INVALID", "no such step", id=step_id)
    container, index = found
    container[index] = {**container[index], "enabled": enabled}
    return _validated(draft)


def update_step(recording: Mapping[str, Any], step_id: str,
                changes: Mapping[str, Any]) -> dict[str, Any]:
    """Apply field changes to one step, refusing anything not editable.

    An allow-list rather than a deny-list: a new field added to the recording
    format later is not silently editable just because nobody remembered to
    protect it.
    """

    rejected = sorted(set(changes) - EDITABLE_FIELDS)
    if rejected:
        raise EditError(
            "RECORDING.ORDER_INVALID",
            "these fields cannot be edited",
            id=step_id,
            fields=rejected,
            # Naming the protected ones separately tells the operator the
            # difference between "unknown field" and "deliberately immutable".
            protected=sorted(set(rejected) & PROTECTED_FIELDS),
        )

    draft = copy.deepcopy(dict(recording))
    found = _locate(draft.setdefault("steps", []), step_id)
    if found is None:
        raise EditError("RECORDING.ORDER_INVALID", "no such step", id=step_id)
    container, index = found
    updated = {**container[index], **copy.deepcopy(dict(changes))}

    # Editing a locator invalidates any verification that was done against the
    # old one.  Leaving `verified: true` would let an edited locator inherit
    # proof it never earned, which is the failure verify_locators exists to stop.
    if "locator" in changes and updated.get("kind") == "interaction":
        updated["disambiguation"] = {"strategy": "unresolved", "verified": False}

    container[index] = updated
    return _validated(draft)


def reorder(recording: Mapping[str, Any], order: Sequence[str]
            ) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    """Reorder top-level steps, returning the result and any warnings.

    The new order must be a permutation of the existing top-level ids: accepting
    a partial list would silently drop whatever was omitted.
    """

    draft = copy.deepcopy(dict(recording))
    steps = draft.setdefault("steps", [])
    existing = [step["id"] for step in steps]
    if sorted(order) != sorted(existing):
        raise EditError(
            "RECORDING.ORDER_INVALID",
            "order must list every top-level step exactly once",
            expected=sorted(existing),
            received=sorted(order),
        )

    by_id = {step["id"]: step for step in steps}
    draft["steps"] = [by_id[step_id] for step_id in order]
    validated = _validated(draft)
    return validated, order_warnings(validated)


def insert_logic(recording: Mapping[str, Any], step_id: str, when: str,
                 wrap: Sequence[str] = ()) -> dict[str, Any]:
    """Wrap existing top-level steps in a condition.

    Wrapping rather than inserting an empty shell: a condition with no body
    changes nothing, so the operator would have to perform a second, separate
    move to make it meaningful -- and that intermediate state is exactly where a
    reference can be left dangling.
    """

    draft = copy.deepcopy(dict(recording))
    steps = draft.setdefault("steps", [])
    if step_id in _steps_index(steps):
        raise EditError("RECORDING.ORDER_INVALID", "step id already exists",
                        id=step_id)

    wrapped = list(wrap)
    known = [step["id"] for step in steps]
    unknown = sorted(set(wrapped) - set(known))
    if unknown:
        raise EditError("RECORDING.ORDER_INVALID",
                        "cannot wrap steps that are not top-level",
                        ids=unknown)

    body = [step for step in steps if step["id"] in set(wrapped)]
    remaining = [step for step in steps if step["id"] not in set(wrapped)]
    node = {
        "id": step_id,
        "kind": "logic",
        "logic": {"type": "condition", "when": when},
        "steps": body,
    }
    # Placed where the first wrapped step was, so wrapping does not itself
    # reorder the flow.
    position = next((index for index, step in enumerate(steps)
                     if step["id"] in set(wrapped)), len(remaining))
    remaining.insert(min(position, len(remaining)), node)
    draft["steps"] = remaining
    return _validated(draft)


def declare_input(recording: Mapping[str, Any], name: str,
                  spec: Mapping[str, Any]) -> dict[str, Any]:
    """Declare an input.

    Needed because a condition expression referencing an undeclared input is
    refused by the compiler; without this the UI could offer logic editing that
    can never be committed.
    """

    if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name):
        raise EditError("RECORDING.ORDER_INVALID", "invalid input name",
                        name=name)
    draft = copy.deepcopy(dict(recording))
    draft.setdefault("inputs", {})[name] = dict(spec)
    return _validated(draft)


def step_view(recording: Mapping[str, Any]) -> list[dict[str, Any]]:
    """Flatten steps for display, keeping nesting visible.

    The disambiguation state is surfaced per step because the driver fails
    closed on ambiguity: "this locator matched three nodes" is something the
    operator must see while editing, not discover as DRIVER.AMBIGUOUS at replay.
    """

    rows: list[dict[str, Any]] = []

    def walk(steps: Iterable[Mapping[str, Any]], depth: int,
             parent: str | None) -> None:
        for step in steps:
            disambiguation = step.get("disambiguation") or {}
            rows.append({
                "id": step["id"],
                "kind": step.get("kind"),
                "action": step.get("action"),
                "depth": depth,
                "parent": parent,
                "enabled": step.get("enabled", True),
                "locator": step.get("locator"),
                "window": step.get("window"),
                "of_step": step.get("of_step"),
                "logic": step.get("logic"),
                "strategy": disambiguation.get("strategy"),
                "verified": bool(disambiguation.get("verified")),
                "observed": step.get("observed"),
            })
            if step.get("kind") == "logic":
                walk(step.get("steps", []), depth + 1, step["id"])

    walk(recording.get("steps", []), 0, None)
    return rows


def compile_preview(recording: Mapping[str, Any]) -> dict[str, Any]:
    """Compile for display, reporting failure instead of raising.

    The UI needs to show why a recording cannot compile while the operator is
    still editing it; an exception would only surface as a failed request.
    """

    try:
        workflow = compile_recording(copy.deepcopy(dict(recording)))
    except RecordingError as exc:
        return {"ok": False, "error": exc.to_dict()}
    return {
        "ok": True,
        "steps": [step["id"] for step in workflow["steps"]],
        "permissions": workflow["requires"].get("permissions", []),
        "workflow": workflow,
    }
