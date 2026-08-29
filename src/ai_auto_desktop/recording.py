"""Compile a Recording artifact into a Workflow descriptor.

The spec (docs/spec/recording-session-v1alpha1.md) defines the Recording format
and its compilation, but nothing implemented it: the tracked example and its
compiled form are both hand-written, and no code reads either.  This module
closes that gap so a captured session can actually become something replayable.

Direction is one-way by design (spec section 8).  A Workflow can express far
more than a recording can represent, so there is no reverse compilation.

Three things this module must do that the existing workflow compiler does not:

* Expand one interaction into snapshot -> find -> action.  A recording stores a
  locator, but driver actions need a session-scoped target, so the target has to
  be produced during replay rather than baked in.

* Attach assertions as postconditions of the step they check, never as separate
  steps, so that a failed assertion fails the action it belongs to.

* Check reference integrity itself.  Measured against the real compiler: a
  descriptor referencing a deleted step id is accepted, because the existing
  rule is "a reference must be covered by depends_on" and a missing step
  produces no dependency.  Recording edits delete and reorder steps, so this
  gap has to be closed here or dangling references reach replay.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
import json
import re
from typing import Any

RECORDING_API_VERSION = "ai-auto-desktop.dev/v1alpha1"
RECORDING_KIND = "Recording"

# A recorded action costs three executed steps once expanded, so the default
# budget is derived from the step count rather than guessed.
STEPS_PER_INTERACTION = 3
DEFAULT_MAX_DURATION = "5m"
MIN_EXECUTED_STEPS = 20

STEP_KINDS = frozenset({"interaction", "assertion", "logic"})
DISAMBIGUATION_STRATEGIES = frozenset({"unique", "scoped", "ordinal", "unresolved"})


class RecordingError(Exception):
    """A structured recording-domain failure.

    The code namespace is deliberately separate from the workflow runtime's, so
    that a recording that cannot be compiled is never confused with a workflow
    that failed to run.
    """

    def __init__(self, code: str, message: str, **details: Any) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.details = details

    def to_dict(self) -> dict[str, Any]:
        return {"code": self.code, "message": self.message, "details": self.details}


def _fail(code: str, message: str, **details: Any) -> None:
    raise RecordingError(code, message, **details)


def _mapping(value: Any, name: str) -> Mapping[str, Any]:
    if not isinstance(value, Mapping):
        _fail("RECORDING.INVALID", f"{name} must be an object")
    return value


def _sequence(value: Any, name: str) -> Sequence[Any]:
    if not isinstance(value, Sequence) or isinstance(value, (str, bytes)):
        _fail("RECORDING.INVALID", f"{name} must be an array")
    return value


def _enabled(step: Mapping[str, Any]) -> bool:
    value = step.get("enabled", True)
    if not isinstance(value, bool):
        _fail("RECORDING.INVALID", f"step {step.get('id')!r} enabled must be a boolean")
    return value


def _walk(steps: Sequence[Any]) -> list[Mapping[str, Any]]:
    """Every step including nested logic children, in document order."""

    found: list[Mapping[str, Any]] = []
    for entry in steps:
        step = _mapping(entry, "step")
        found.append(step)
        if step.get("kind") == "logic":
            found.extend(_walk(_sequence(step.get("steps", []), "logic.steps")))
    return found


class RecordingCompiler:
    """Recording -> Workflow descriptor."""

    def __init__(self, recording: Mapping[str, Any]) -> None:
        self.recording = _mapping(recording, "recording")
        self.warnings: list[dict[str, Any]] = []

    # ---- validation -----------------------------------------------------

    def _validate_envelope(self) -> None:
        api_version = self.recording.get("apiVersion")
        if api_version != RECORDING_API_VERSION:
            _fail(
                "RECORDING.VERSION_UNSUPPORTED",
                "unsupported recording apiVersion",
                found=api_version,
                supported=RECORDING_API_VERSION,
            )
        if self.recording.get("kind") != RECORDING_KIND:
            _fail(
                "RECORDING.INVALID",
                "descriptor is not a Recording",
                found=self.recording.get("kind"),
            )
        metadata = _mapping(self.recording.get("metadata"), "metadata")
        if not isinstance(metadata.get("name"), str) or not metadata["name"]:
            _fail("RECORDING.INVALID", "metadata.name is required")

    def _validate_platform(self) -> str:
        binding = _mapping(self.recording.get("platform_binding"), "platform_binding")
        platform = binding.get("platform")
        replay = binding.get("replay_platforms")
        if not isinstance(platform, str) or not platform:
            _fail("RECORDING.INVALID", "platform_binding.platform is required")
        replay_list = list(_sequence(replay, "platform_binding.replay_platforms"))
        if not replay_list:
            _fail("RECORDING.INVALID", "replay_platforms must not be empty")
        # A recording captures one platform's semantics; replaying it elsewhere
        # would silently reinterpret locators that do not transfer.
        if replay_list != [platform]:
            _fail(
                "RECORDING.PLATFORM_MISMATCH",
                "a recording may only replay on the platform it was captured on",
                platform=platform,
                replay_platforms=replay_list,
            )
        capture = _mapping(self.recording.get("capture"), "capture")
        captured_platform = capture.get("platform")
        if captured_platform != platform:
            _fail(
                "RECORDING.PLATFORM_MISMATCH",
                "capture.platform disagrees with platform_binding.platform",
                capture=captured_platform,
                binding=platform,
            )
        return platform

    def _validate_redaction(self) -> Mapping[str, Any]:
        redaction = _mapping(self.recording.get("redaction"), "redaction")
        for field in ("value_policy", "title_policy"):
            if redaction.get(field) not in {"drop", "keep"}:
                _fail(
                    "RECORDING.REDACTION_INVALID",
                    f"redaction.{field} must be 'drop' or 'keep'",
                    found=redaction.get(field),
                )
        disclosed = list(_sequence(redaction.get("disclosed", []), "redaction.disclosed"))
        for name in disclosed:
            if not isinstance(name, str):
                _fail("RECORDING.REDACTION_INVALID", "disclosed entries must be strings")
        return redaction

    def _validate_steps(self, steps: Sequence[Any]) -> None:
        if not steps:
            _fail("RECORDING.EMPTY", "a recording must contain at least one step")

        all_steps = _walk(steps)
        seen: set[str] = set()
        for step in all_steps:
            step_id = step.get("id")
            if not isinstance(step_id, str) or not step_id:
                _fail("RECORDING.INVALID", "every step needs a non-empty id")
            if step_id in seen:
                _fail("RECORDING.INVALID", "duplicate step id", id=step_id)
            seen.add(step_id)
            if step.get("kind") not in STEP_KINDS:
                _fail(
                    "RECORDING.INVALID",
                    "unknown step kind",
                    id=step_id,
                    kind=step.get("kind"),
                )

        live = {s["id"] for s in all_steps if _enabled(s)}

        for step in all_steps:
            if not _enabled(step):
                continue
            if step["kind"] == "interaction":
                self._validate_interaction(step)
            elif step["kind"] == "assertion":
                self._validate_assertion(step, live, seen)

    def _validate_interaction(self, step: Mapping[str, Any]) -> None:
        if not isinstance(step.get("action"), str) or not step["action"]:
            _fail("RECORDING.INVALID", "interaction needs an action", id=step["id"])
        locator = _mapping(step.get("locator"), f"step {step['id']} locator")
        if not locator:
            _fail("RECORDING.INVALID", "locator must not be empty", id=step["id"])

        disambiguation = _mapping(
            step.get("disambiguation"), f"step {step['id']} disambiguation"
        )
        strategy = disambiguation.get("strategy")
        if strategy not in DISAMBIGUATION_STRATEGIES:
            _fail(
                "RECORDING.INVALID",
                "unknown disambiguation strategy",
                id=step["id"],
                strategy=strategy,
            )

        # An unresolved locator must never be compiled optimistically: the
        # driver fails closed on ambiguity, so this would fail at replay with a
        # far less useful message.
        if strategy == "unresolved":
            _fail(
                "RECORDING.LOCATOR_UNRESOLVED",
                "step has an unresolved locator and must be fixed or disabled",
                id=step["id"],
            )
        if strategy == "ordinal" and not disambiguation.get("fragile"):
            _fail(
                "RECORDING.LOCATOR_FRAGILE",
                "ordinal disambiguation must be marked fragile",
                id=step["id"],
            )
        if not disambiguation.get("verified"):
            self.warnings.append(
                {
                    "code": "RECORDING.LOCATOR_FRAGILE",
                    "id": step["id"],
                    "message": "locator uniqueness was never verified against a snapshot",
                }
            )
        if strategy == "scoped" and not disambiguation.get("scope"):
            _fail(
                "RECORDING.INVALID",
                "scoped disambiguation must record the scope locator",
                id=step["id"],
            )

        if step["action"] == "type_text":
            self._validate_text(step)

    def _validate_text(self, step: Mapping[str, Any]) -> None:
        text = _mapping(step.get("text"), f"step {step['id']} text")
        source = text.get("source")
        if source not in {"input", "literal"}:
            _fail(
                "RECORDING.INVALID",
                "text.source must be 'input' or 'literal'",
                id=step["id"],
            )
        if source == "input":
            if not isinstance(text.get("input"), str) or not text["input"]:
                _fail("RECORDING.INVALID", "text.input is required", id=step["id"])
            return
        # A literal keeps whatever the user typed in the artifact, so it has to
        # be declared explicitly rather than slipping through as a default.
        disclosed = list(
            _sequence(
                _mapping(self.recording.get("redaction"), "redaction").get(
                    "disclosed", []
                ),
                "redaction.disclosed",
            )
        )
        if step["id"] not in disclosed:
            _fail(
                "RECORDING.REDACTION_INVALID",
                "literal text must be registered in redaction.disclosed",
                id=step["id"],
            )

    def _validate_assertion(
        self, step: Mapping[str, Any], live: set[str], known: set[str]
    ) -> None:
        of_step = step.get("of_step")
        if not isinstance(of_step, str) or not of_step:
            _fail("RECORDING.INVALID", "assertion needs of_step", id=step["id"])
        # Closing the gap the workflow compiler leaves open: a reference to a
        # deleted or disabled step is accepted there, and would reach replay.
        # The two failures are different problems for whoever has to fix the
        # recording: a deleted step leaves a dangling reference that must be
        # repaired or removed, while a disabled step only needs re-enabling.
        # They therefore carry distinct reasons rather than one shared message.
        if of_step not in known:
            _fail(
                "RECORDING.ORDER_INVALID",
                "assertion refers to a step that does not exist",
                id=step["id"],
                of_step=of_step,
                reason="missing",
            )
        if of_step not in live:
            _fail(
                "RECORDING.ORDER_INVALID",
                "assertion refers to a disabled step",
                id=step["id"],
                of_step=of_step,
                reason="disabled",
            )
        observe = _mapping(step.get("observe"), f"step {step['id']} observe")
        if observe.get("action") not in {"find", "snapshot"}:
            _fail(
                "RECORDING.INVALID",
                "assertion.observe.action must be find or snapshot",
                id=step["id"],
            )
        expect = _mapping(step.get("expect"), f"step {step['id']} expect")
        if not expect.get("mode"):
            _fail("RECORDING.INVALID", "assertion.expect.mode is required", id=step["id"])

    # ---- compilation ----------------------------------------------------

    def compile(self) -> dict[str, Any]:
        """Produce a Workflow descriptor, or raise RecordingError."""

        self._validate_envelope()
        platform = self._validate_platform()
        self._validate_redaction()
        steps = list(_sequence(self.recording.get("steps"), "steps"))
        self._validate_steps(steps)

        driver = _mapping(
            _mapping(self.recording.get("capture"), "capture").get("driver"),
            "capture.driver",
        )
        driver_name = driver.get("name")
        if not isinstance(driver_name, str) or not driver_name:
            _fail("RECORDING.INVALID", "capture.driver.name is required")

        all_steps = _walk(steps)
        assertions: dict[str, list[Mapping[str, Any]]] = {}
        for step in all_steps:
            if step["kind"] == "assertion" and _enabled(step):
                assertions.setdefault(step["of_step"], []).append(step)

        compiled = self._compile_steps(steps, driver_name, assertions)

        # Counts nested logic children too: a condition body still costs
        # executed steps at replay.
        interactions = sum(
            1 for s in all_steps if s["kind"] == "interaction" and _enabled(s)
        )
        if not compiled:
            _fail("RECORDING.EMPTY", "every step is disabled; nothing to compile")

        metadata = dict(_mapping(self.recording.get("metadata"), "metadata"))
        metadata.pop("description", None)
        annotations = dict(metadata.get("annotations") or {})
        # The recorded order is preserved for the UI to show drift, but it does
        # not take part in execution: list order is the only ordering truth.
        annotations.setdefault(
            "ai-auto-desktop.dev/recorded-order",
            ",".join(s["id"] for s in steps if isinstance(s, Mapping)),
        )
        metadata["annotations"] = annotations

        workflow: dict[str, Any] = {
            "apiVersion": RECORDING_API_VERSION,
            "kind": "Workflow",
            "metadata": metadata,
            "requires": {
                "platforms": [platform],
                "capabilities": [
                    {
                        "name": driver_name,
                        "version": f">={driver.get('version', '0.1.0')}",
                    }
                ],
            },
            "budgets": {
                "max_duration": DEFAULT_MAX_DURATION,
                # One recorded action costs three executed steps, so the budget
                # is derived rather than guessed.
                "max_executed_steps": max(
                    MIN_EXECUTED_STEPS, interactions * STEPS_PER_INTERACTION * 2
                ),
            },
            "steps": compiled,
        }

        inputs = self._collect_inputs(all_steps)
        if inputs:
            workflow["inputs"] = inputs
        self._check_input_references(workflow, inputs)
        return workflow

    def _collect_inputs(self, all_steps: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
        """Declare an input for every externalized text value.

        Recorded text is externalized by default, so the compiled workflow has
        to declare the inputs it now depends on.
        """

        inputs: dict[str, Any] = {}
        declared = _mapping(self.recording.get("inputs", {}) or {}, "inputs")
        for name, definition in declared.items():
            inputs[name] = dict(_mapping(definition, f"inputs.{name}"))
        for step in all_steps:
            if not _enabled(step) or step["kind"] != "interaction":
                continue
            text = step.get("text")
            if not isinstance(text, Mapping) or text.get("source") != "input":
                continue
            name = text["input"]
            entry = inputs.setdefault(
                name, {"schema": {"type": "string"}, "required": True}
            )
            if text.get("sensitive"):
                entry["sensitive"] = True
        return inputs

    def _check_input_references(
        self, workflow: Mapping[str, Any], declared: Mapping[str, Any]
    ) -> None:
        """Every ${{ inputs.X }} must resolve to a declared input.

        Measured: the workflow compiler accepts references to inputs that were
        never declared, and to inputs deleted after the fact.  Since hand
        editing a recording is an explicit feature, that reference would only
        surface at replay -- so it is checked here, exactly like step
        references.
        """

        blob = json.dumps(workflow, ensure_ascii=False)
        referenced = set(re.findall(r"inputs\.([A-Za-z_][A-Za-z0-9_]*)", blob))
        missing = sorted(referenced - set(declared))
        if missing:
            _fail(
                "RECORDING.ORDER_INVALID",
                "workflow references inputs that are not declared",
                inputs=missing,
            )

    def _compile_steps(
        self,
        steps: Sequence[Any],
        driver: str,
        assertions: Mapping[str, list[Mapping[str, Any]]],
    ) -> list[dict[str, Any]]:
        compiled: list[dict[str, Any]] = []
        for entry in steps:
            step = _mapping(entry, "step")
            if not _enabled(step):
                # Disabled steps stay in the artifact but never compile; this is
                # what makes non-destructive editing possible.
                continue
            kind = step["kind"]
            if kind == "assertion":
                continue  # attached to its target, never standalone
            if kind == "interaction":
                compiled.extend(self._compile_interaction(step, driver, assertions))
            elif kind == "logic":
                compiled.append(self._compile_logic(step, driver, assertions))
        return compiled

    def _compile_interaction(
        self,
        step: Mapping[str, Any],
        driver: str,
        assertions: Mapping[str, list[Mapping[str, Any]]],
    ) -> list[dict[str, Any]]:
        """Expand to snapshot -> find -> action.

        The recording holds a locator, but every driver action needs a target
        that is only valid inside one session, so the target must be resolved
        during replay rather than stored.
        """

        step_id = step["id"]
        locator = dict(step["locator"])
        window = dict(step.get("window") or {})
        snapshot_id = f"{step_id}__snapshot"
        find_id = f"{step_id}__find"

        snapshot_step: dict[str, Any] = {
            "id": snapshot_id,
            "type": "action",
            "uses": f"{driver}.snapshot@1",
            "with": {"window": window} if window else {"window": {}},
            "effect": {"class": "read_only"},
            "risk": {"category": "observe", "level": "low"},
        }
        find_step: dict[str, Any] = {
            "id": find_id,
            "type": "action",
            "uses": f"{driver}.find@1",
            "with": {
                "snapshot_id": f"${{{{ steps.{snapshot_id}.output.snapshot_id }}}}",
                "revision": f"${{{{ steps.{snapshot_id}.output.revision }}}}",
                "locator": locator,
            },
            "effect": {"class": "read_only"},
            "risk": {"category": "observe", "level": "low"},
        }

        action_with: dict[str, Any] = {
            "target": f"${{{{ steps.{find_id}.output.target }}}}",
            "locator": locator,
        }
        text = step.get("text")
        if isinstance(text, Mapping):
            if text.get("source") == "input":
                action_with["text"] = f"${{{{ inputs.{text['input']} }}}}"
            else:
                action_with["text"] = text.get("value", "")
        if "value" in step:
            action_with["value"] = step["value"]

        action_step: dict[str, Any] = {
            "id": step_id,
            "type": "action",
            "uses": f"{driver}.{step['action']}@1",
            "with": action_with,
            "effect": {"class": _effect_class(step["action"])},
            "risk": _risk_for(step["action"]),
        }

        attached = assertions.get(step_id) or []
        if attached:
            if len(attached) > 1:
                _fail(
                    "RECORDING.INVALID",
                    "a step may carry at most one assertion",
                    id=step_id,
                    count=len(attached),
                )
            action_step["postcondition"] = _compile_assertion(attached[0], driver)
        return [snapshot_step, find_step, action_step]

    def _compile_logic(
        self,
        step: Mapping[str, Any],
        driver: str,
        assertions: Mapping[str, list[Mapping[str, Any]]],
    ) -> dict[str, Any]:
        logic = _mapping(step.get("logic"), f"step {step['id']} logic")
        logic_type = logic.get("type")
        if logic_type != "condition":
            _fail(
                "RECORDING.LOGIC_UNSUPPORTED",
                "only condition logic is supported",
                id=step["id"],
                type=logic_type,
            )
        when = logic.get("when")
        if not isinstance(when, str) or not when:
            _fail("RECORDING.INVALID", "condition logic needs 'when'", id=step["id"])
        children = self._compile_steps(
            _sequence(step.get("steps", []), "logic.steps"), driver, assertions
        )
        if not children:
            _fail(
                "RECORDING.EMPTY",
                "condition logic has no enabled steps",
                id=step["id"],
            )
        return {
            "id": step["id"],
            "type": "if",
            "condition": when,
            "then": children,
        }


def _compile_assertion(step: Mapping[str, Any], driver: str) -> dict[str, Any]:
    """Attach an assertion as a postcondition.

    Never a standalone step: a check that runs as its own step can pass while
    the action it was meant to verify has already failed.
    """

    observe = _mapping(step["observe"], "assertion.observe")
    expect = _mapping(step["expect"], "assertion.expect")
    mode = expect["mode"]
    if mode == "exists":
        condition = "${{ observation.found }}"
    elif mode == "absent":
        condition = "${{ not observation.found }}"
    else:
        _fail(
            "RECORDING.INVALID",
            "unsupported assertion mode",
            id=step["id"],
            mode=mode,
        )

    payload: dict[str, Any] = {
        "observe": {
            "uses": f"{driver}.{observe['action']}@1",
            "with": {"locator": dict(observe.get("locator") or {})},
        },
        "condition": condition,
    }
    if step.get("timeout"):
        payload["timeout"] = step["timeout"]
    if step.get("poll_interval"):
        payload["poll_interval"] = step["poll_interval"]
    return payload


def _effect_class(action: str) -> str:
    return {
        "focus": "idempotent",
        "invoke": "non_idempotent",
        "set_value": "idempotent",
        "type_text": "non_idempotent",
        "pointer_click": "non_idempotent",
    }.get(action, "non_idempotent")


def _risk_for(action: str) -> dict[str, str]:
    if action == "focus":
        return {"category": "observe", "level": "low"}
    return {"category": "input", "level": "medium"}


def compile_recording(recording: Mapping[str, Any]) -> dict[str, Any]:
    """Compile a Recording mapping into a Workflow descriptor."""

    return RecordingCompiler(recording).compile()
# ---------------------------------------------------------------------------
# Capture events -> Recording
# ---------------------------------------------------------------------------

# How a captured event kind becomes a replayable action.  focus_changed is
# absent on purpose: focus follows from the action that caused it, so recording
# it separately would replay a focus the user never deliberately performed.
EVENT_ACTIONS = {
    "invoked": "invoke",
    "value_changed": "type_text",
    "selection_changed": "focus",
}

# UIA control type ids -> the role vocabulary the locator schema uses.  Only ids
# actually seen from the capture layer are mapped; anything else is reported
# rather than guessed at, because a wrong role produces a locator that silently
# matches the wrong element.
CONTROL_TYPE_ROLES = {
    50000: "button",
    50004: "edit",
    50020: "text",
    50026: "group",
    50032: "window",
    50033: "pane",
    50036: "document",
    50002: "checkbox",
    50003: "combobox",
    50005: "hyperlink",
    50008: "listitem",
    50011: "menuitem",
    50024: "tabitem",
}

# Locator fields are added in this order and no further, because measured
# uniqueness stops improving after class_name (87.0% -> 87.0%) while every extra
# field makes the locator more brittle against UI change.
NARROWING_ORDER = ("name", "class_name", "automation_id", "framework_id")


def _element_identity(element: Mapping[str, Any]) -> tuple:
    """Identity used to decide whether two events concern the same control."""

    return (
        element.get("role_id"),
        element.get("automation_id"),
        element.get("class_name"),
        element.get("name"),
        element.get("process_id"),
    )


def _coalesce(events: Sequence[Mapping[str, Any]]) -> list[Mapping[str, Any]]:
    """Collapse consecutive value changes for the same element into one.

    Measured: a single edit raises exactly two value_changed events, because
    TextChanged and the Value property change are both subscribed.  Emitting a
    step per event would type the text twice on replay.

    Only *consecutive* runs collapse.  Any other interaction in between ends the
    run, so edit / click / edit still produces three steps in the right order --
    verified against a real window.
    """

    collapsed: list[Mapping[str, Any]] = []
    for event in events:
        if not isinstance(event, Mapping):
            collapsed.append(event)
            continue
        if event.get("kind") != "value_changed" or not collapsed:
            collapsed.append(event)
            continue
        previous = collapsed[-1]
        same_kind = previous.get("kind") == "value_changed"
        same_element = _element_identity(
            previous.get("element") or {}
        ) == _element_identity(event.get("element") or {})
        if same_kind and same_element:
            # Keep the first: its sequence number marks where the user's edit
            # began, which is what the step ordering should reflect.
            continue
        collapsed.append(event)
    return collapsed


class EventConverter:
    """Turn a stream of captured events into Recording steps.

    The converter is deliberately conservative: an event it cannot turn into a
    replayable step is reported, never dropped and never guessed at.  A recorder
    that silently discards interactions produces a recording that looks complete
    and replays wrong.
    """

    def __init__(
        self,
        *,
        window: Mapping[str, Any] | None = None,
        platform: str = "windows",
        driver: str = "desktop.windows_uia",
        driver_version: str = "0.1.0",
    ) -> None:
        self.window = dict(window or {})
        self.platform = platform
        self.driver = driver
        self.driver_version = driver_version
        self.skipped: list[dict[str, Any]] = []
        self._used_ids: set[str] = set()

    def convert(
        self,
        events: Sequence[Mapping[str, Any]],
        *,
        name: str,
        recorded_at: str,
        probe: Mapping[str, Any] | None = None,
        environment: Mapping[str, Any] | None = None,
        dropped_events: int = 0,
    ) -> dict[str, Any]:
        # A recording built from a lossy capture would be silently incomplete,
        # and the loss is invisible once the events are gone.
        if dropped_events:
            _fail(
                "RECORDING.INVALID",
                "capture reported dropped events; the recording would be incomplete",
                dropped_events=dropped_events,
            )

        steps: list[dict[str, Any]] = []
        inputs: dict[str, Any] = {}
        for event in _coalesce(events):
            step = self._convert_event(_mapping(event, "event"), inputs)
            if step is not None:
                steps.append(step)

        if not steps:
            _fail(
                "RECORDING.EMPTY",
                "no captured event could be turned into a replayable step",
                skipped=self.skipped,
            )

        recording: dict[str, Any] = {
            "apiVersion": RECORDING_API_VERSION,
            "kind": RECORDING_KIND,
            "metadata": {
                "name": name,
                "version": "0.1.0",
                "annotations": {
                    "ai-auto-desktop.dev/recorded-order": ",".join(
                        s["id"] for s in steps
                    )
                },
            },
            "capture": {
                "platform": self.platform,
                "recorded_at": recorded_at,
                "driver": {"name": self.driver, "version": self.driver_version},
                "probe": dict(probe or {}),
                "environment": dict(environment or {}),
            },
            # Values are never captured in the first place, so dropping them is
            # a statement of fact rather than a policy applied after the event.
            "redaction": {
                "value_policy": "drop",
                "title_policy": "drop",
                "screenshots": "none",
                "disclosed": [],
            },
            "platform_binding": {
                "platform": self.platform,
                "replay_platforms": [self.platform],
            },
            "steps": steps,
        }
        if inputs:
            recording["inputs"] = inputs
        return recording

    def _convert_event(
        self, event: Mapping[str, Any], inputs: dict[str, Any]
    ) -> dict[str, Any] | None:
        kind = event.get("kind")
        action = EVENT_ACTIONS.get(kind)
        if action is None:
            # focus_changed lands here by design; anything else is unexpected
            # and is recorded as skipped rather than dropped.
            self._skip(event, "no replayable action for this event kind")
            return None

        element = _mapping(event.get("element"), "event.element")
        role = self._role(element)
        if role is None:
            self._skip(event, "unknown control type; role cannot be determined")
            return None

        locator = self._locator(role, element)
        step_id = self._step_id(action, element, role)
        step: dict[str, Any] = {
            "id": step_id,
            "kind": "interaction",
            "action": action,
            "locator": locator,
            # The recorder has not resolved this against a snapshot yet, so it
            # must say so: claiming verified uniqueness without checking is the
            # failure the disambiguation rules exist to prevent.
            "disambiguation": {"strategy": "unique", "verified": False},
            "observed": {
                "role": role,
                "had_value": kind == "value_changed",
                "provenance": {
                    key: element[key]
                    for key in ("framework_id", "process_id")
                    if element.get(key) is not None
                },
            },
        }
        if self.window:
            step["window"] = dict(self.window)

        if action == "type_text":
            # The text itself was never captured, so it becomes an input the
            # operator fills in -- which is also what the redaction policy
            # requires.
            input_name = f"{step_id}_text"
            step["text"] = {
                "source": "input",
                "input": input_name,
                "sensitive": True,
            }
            inputs[input_name] = {
                "schema": {"type": "string"},
                "required": True,
                "sensitive": True,
            }
        return step

    def _role(self, element: Mapping[str, Any]) -> str | None:
        role_id = element.get("role_id")
        if not isinstance(role_id, int):
            return None
        return CONTROL_TYPE_ROLES.get(role_id)

    def _locator(self, role: str, element: Mapping[str, Any]) -> dict[str, Any]:
        """Narrow only until the locator is specific, then stop.

        Measured: adding fields past class_name does not improve uniqueness but
        does reduce tolerance to UI change.
        """

        locator: dict[str, Any] = {"role": role}
        for field in NARROWING_ORDER:
            value = element.get(field)
            if isinstance(value, str) and value:
                locator[field] = value
                if field == "class_name":
                    break
        return locator

    def _step_id(self, action: str, element: Mapping[str, Any], role: str) -> str:
        label = element.get("name") or element.get("automation_id") or role
        slug = re.sub(r"[^a-z0-9]+", "_", str(label).lower()).strip("_")
        base = f"{action}_{slug or role}"[:48]
        candidate = base
        suffix = 2
        while candidate in self._used_ids:
            candidate = f"{base}_{suffix}"
            suffix += 1
        self._used_ids.add(candidate)
        return candidate

    def _skip(self, event: Mapping[str, Any], reason: str) -> None:
        self.skipped.append(
            {
                "sequence": event.get("sequence"),
                "kind": event.get("kind"),
                "reason": reason,
            }
        )


def convert_events(
    events: Sequence[Mapping[str, Any]],
    *,
    name: str,
    recorded_at: str,
    window: Mapping[str, Any] | None = None,
    platform: str = "windows",
    driver: str = "desktop.windows_uia",
    probe: Mapping[str, Any] | None = None,
    environment: Mapping[str, Any] | None = None,
    dropped_events: int = 0,
) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    """Convert captured events into a Recording.

    Returns the recording and the list of events that could not be converted.
    The caller is expected to surface the skipped list: an interaction that
    produced no step is exactly what the operator needs to know about.
    """

    converter = EventConverter(
        window=window, platform=platform, driver=driver
    )
    recording = converter.convert(
        events,
        name=name,
        recorded_at=recorded_at,
        probe=probe,
        environment=environment,
        dropped_events=dropped_events,
    )
    return recording, converter.skipped
