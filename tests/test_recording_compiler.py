"""Tests for the recording compiler and the capture-event converter.

These run anywhere: the converter is fed event dictionaries shaped exactly like
the ones measured from a real window, and the compiled output is checked by the
project's own workflow compiler rather than by assertions I invented.

The behaviours pinned here are the ones measurement forced:

* one edit raises two value_changed events, so consecutive ones must collapse or
  replay types the text twice;
* the workflow compiler accepts references to steps and inputs that do not
  exist, so the recording compiler has to reject them itself;
* a step-less workflow is invalid, so an empty recording must fail rather than
  compile to nothing.
"""

from __future__ import annotations

import copy
from pathlib import Path
import unittest

from ai_auto_desktop.compiler import compile_descriptor
from ai_auto_desktop.recording import (
    RecordingError,
    _coalesce,
    compile_recording,
    convert_events,
)

PROJECT_ROOT = Path(__file__).resolve().parents[1]
EXAMPLE = PROJECT_ROOT / "examples" / "recordings" / "save-note.recording.yaml"


def event(sequence: int, kind: str, **element: object) -> dict:
    """An event shaped like the capture layer's real output."""

    base = {
        "role_id": 50000,
        "name": "Save",
        "class_name": "Button",
        "automation_id": "btn1",
        "framework_id": "Win32",
        "process_id": 4242,
    }
    base.update(element)
    return {"kind": kind, "sequence": sequence, "element": base}


def minimal_recording(**overrides: object) -> dict:
    recording = {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "Recording",
        "metadata": {"name": "demo", "version": "0.1.0"},
        "capture": {
            "platform": "windows",
            "recorded_at": "2026-08-30T00:00:00Z",
            "driver": {"name": "desktop.windows_uia", "version": "0.1.0"},
        },
        "redaction": {
            "value_policy": "drop",
            "title_policy": "drop",
            "screenshots": "none",
            "disclosed": [],
        },
        "platform_binding": {"platform": "windows", "replay_platforms": ["windows"]},
        "steps": [
            {
                "id": "click_save",
                "kind": "interaction",
                "action": "invoke",
                "locator": {"role": "button", "name": "Save"},
                "disambiguation": {"strategy": "unique", "verified": True},
            }
        ],
    }
    recording.update(overrides)
    return recording


class CoalesceTests(unittest.TestCase):
    """One edit produces two events; replay must not type twice."""

    def test_consecutive_value_changes_collapse(self) -> None:
        # Exactly the sequence measured from a real edit control.
        events = [
            event(1, "focus_changed", role_id=50004, class_name="Edit"),
            event(2, "value_changed", role_id=50004, class_name="Edit"),
            event(3, "value_changed", role_id=50004, class_name="Edit"),
        ]
        kinds = [e["kind"] for e in _coalesce(events)]
        self.assertEqual(kinds, ["focus_changed", "value_changed"])

    def test_an_interleaved_action_keeps_two_edits_apart(self) -> None:
        # Measured: edit / invoke / edit must stay three distinct actions.
        events = [
            event(1, "value_changed", role_id=50004, automation_id="1001"),
            event(2, "value_changed", role_id=50004, automation_id="1001"),
            event(3, "invoked", automation_id="1002"),
            event(4, "value_changed", role_id=50004, automation_id="1001"),
            event(5, "value_changed", role_id=50004, automation_id="1001"),
        ]
        kinds = [e["kind"] for e in _coalesce(events)]
        self.assertEqual(kinds, ["value_changed", "invoked", "value_changed"])

    def test_different_elements_never_merge(self) -> None:
        events = [
            event(1, "value_changed", automation_id="1001"),
            event(2, "value_changed", automation_id="2002"),
        ]
        self.assertEqual(len(_coalesce(events)), 2)

    def test_the_first_event_of_a_run_is_kept(self) -> None:
        # Its sequence marks where the user's edit began.
        events = [
            event(7, "value_changed", role_id=50004),
            event(8, "value_changed", role_id=50004),
        ]
        self.assertEqual(_coalesce(events)[0]["sequence"], 7)


class ConverterTests(unittest.TestCase):
    def convert(self, events, **kwargs):
        return convert_events(
            events, name="demo", recorded_at="2026-08-30T00:00:00Z", **kwargs
        )

    def test_invoke_becomes_an_interaction(self) -> None:
        recording, skipped = self.convert([event(1, "invoked")])
        self.assertEqual(len(recording["steps"]), 1)
        step = recording["steps"][0]
        self.assertEqual(step["action"], "invoke")
        self.assertEqual(step["locator"]["role"], "button")
        self.assertEqual(skipped, [])

    def test_focus_changes_are_skipped_not_dropped(self) -> None:
        # Focus follows from the action that caused it; replaying it separately
        # would perform a focus the user never intended.  But the operator still
        # has to be able to see it was ignored.
        _, skipped = self.convert([event(1, "invoked"), event(2, "focus_changed")])
        self.assertEqual(len(skipped), 1)
        self.assertEqual(skipped[0]["kind"], "focus_changed")
        self.assertIn("reason", skipped[0])

    def test_unknown_control_type_is_reported(self) -> None:
        _, skipped = self.convert(
            [event(1, "invoked"), event(2, "invoked", role_id=99999)]
        )
        self.assertEqual(len(skipped), 1)
        self.assertIn("role", skipped[0]["reason"])

    def test_value_change_becomes_an_input_not_a_literal(self) -> None:
        recording, _ = self.convert(
            [event(1, "value_changed", role_id=50004, class_name="Edit")]
        )
        step = recording["steps"][0]
        self.assertEqual(step["action"], "type_text")
        self.assertEqual(step["text"]["source"], "input")
        self.assertTrue(step["text"]["sensitive"])
        self.assertIn(step["text"]["input"], recording["inputs"])

    def test_locator_stops_narrowing_at_class_name(self) -> None:
        # Measured: extra fields do not improve uniqueness (87.0% -> 87.0%) but
        # do make the locator more brittle.
        recording, _ = self.convert([event(1, "invoked")])
        locator = recording["steps"][0]["locator"]
        self.assertEqual(sorted(locator), ["class_name", "name", "role"])
        self.assertNotIn("automation_id", locator)

    def test_uniqueness_is_not_claimed_without_verification(self) -> None:
        recording, _ = self.convert([event(1, "invoked")])
        self.assertFalse(recording["steps"][0]["disambiguation"]["verified"])

    def test_step_ids_are_unique(self) -> None:
        recording, _ = self.convert(
            [event(1, "invoked"), event(2, "invoked"), event(3, "invoked")]
        )
        ids = [s["id"] for s in recording["steps"]]
        self.assertEqual(len(ids), len(set(ids)))

    def test_dropped_events_refuse_to_convert(self) -> None:
        # A recording built from a lossy capture is silently incomplete.
        with self.assertRaises(RecordingError) as caught:
            self.convert([event(1, "invoked")], dropped_events=3)
        self.assertEqual(caught.exception.code, "RECORDING.INVALID")

    def test_no_convertible_event_is_an_error(self) -> None:
        with self.assertRaises(RecordingError) as caught:
            self.convert([event(1, "focus_changed")])
        self.assertEqual(caught.exception.code, "RECORDING.EMPTY")

    def test_recording_never_carries_a_value(self) -> None:
        recording, _ = self.convert(
            [event(1, "value_changed", role_id=50004, name="secret-content")]
        )
        self.assertEqual(recording["redaction"]["value_policy"], "drop")

    def test_converted_recording_compiles(self) -> None:
        recording, _ = self.convert(
            [event(1, "invoked"), event(2, "value_changed", role_id=50004)]
        )
        workflow = compile_recording(recording)
        # Judged by the project's own compiler, not by my assertions.
        compile_descriptor(workflow, source="test")


class CompilerTests(unittest.TestCase):
    def test_interaction_expands_to_three_steps(self) -> None:
        # A recording holds a locator, but actions need a session-scoped target.
        workflow = compile_recording(minimal_recording())
        ids = [s["id"] for s in workflow["steps"]]
        self.assertEqual(
            ids, ["click_save__snapshot", "click_save__find", "click_save"]
        )

    def test_assertion_is_a_postcondition_not_a_step(self) -> None:
        recording = minimal_recording()
        recording["steps"].append(
            {
                "id": "saved",
                "kind": "assertion",
                "of_step": "click_save",
                "observe": {"action": "find", "locator": {"role": "window"}},
                "expect": {"mode": "exists"},
                "timeout": "5s",
            }
        )
        workflow = compile_recording(recording)
        ids = [s["id"] for s in workflow["steps"]]
        self.assertNotIn("saved", ids)
        action = [s for s in workflow["steps"] if s["id"] == "click_save"][0]
        self.assertIn("postcondition", action)
        self.assertEqual(action["postcondition"]["condition"],
                         "${{ observation.found }}")

    def test_assertion_on_a_missing_step_is_rejected(self) -> None:
        # The workflow compiler accepts this; the recording compiler must not.
        recording = minimal_recording()
        recording["steps"].append(
            {
                "id": "saved",
                "kind": "assertion",
                "of_step": "does_not_exist",
                "observe": {"action": "find", "locator": {"role": "window"}},
                "expect": {"mode": "exists"},
            }
        )
        with self.assertRaises(RecordingError) as caught:
            compile_recording(recording)
        self.assertEqual(caught.exception.code, "RECORDING.ORDER_INVALID")
        # Must be reported as missing, not as disabled: a dangling reference
        # and a switched-off step need different repairs.
        self.assertEqual(caught.exception.details["reason"], "missing")

    def test_assertion_on_a_disabled_step_is_rejected(self) -> None:
        recording = minimal_recording()
        recording["steps"][0]["enabled"] = False
        recording["steps"].append(
            {
                "id": "saved",
                "kind": "assertion",
                "of_step": "click_save",
                "observe": {"action": "find", "locator": {"role": "window"}},
                "expect": {"mode": "exists"},
            }
        )
        with self.assertRaises(RecordingError) as caught:
            compile_recording(recording)
        self.assertEqual(caught.exception.code, "RECORDING.ORDER_INVALID")
        self.assertEqual(caught.exception.details["reason"], "disabled")

    def test_undeclared_input_reference_is_rejected(self) -> None:
        # Measured: the workflow compiler accepts this, so it is checked here.
        recording = minimal_recording()
        recording["steps"] = [
            {
                "id": "maybe",
                "kind": "logic",
                "logic": {"type": "condition", "when": "${{ inputs.ghost }}"},
                "steps": [minimal_recording()["steps"][0]],
            }
        ]
        with self.assertRaises(RecordingError) as caught:
            compile_recording(recording)
        self.assertEqual(caught.exception.code, "RECORDING.ORDER_INVALID")
        self.assertIn("ghost", caught.exception.details["inputs"])

    def test_disabled_steps_are_omitted_but_kept_in_the_artifact(self) -> None:
        recording = minimal_recording()
        recording["steps"].append(
            {
                "id": "second",
                "kind": "interaction",
                "action": "focus",
                "locator": {"role": "edit"},
                "disambiguation": {"strategy": "unique", "verified": True},
                "enabled": False,
            }
        )
        workflow = compile_recording(recording)
        ids = [s["id"] for s in workflow["steps"]]
        self.assertNotIn("second", ids)
        self.assertEqual(len(recording["steps"]), 2)

    def test_all_disabled_is_an_error_not_an_empty_workflow(self) -> None:
        # Measured: workflow.schema.json requires minItems 1 on steps, so a
        # step-less workflow cannot be produced at all.
        recording = minimal_recording()
        recording["steps"][0]["enabled"] = False
        with self.assertRaises(RecordingError) as caught:
            compile_recording(recording)
        self.assertEqual(caught.exception.code, "RECORDING.EMPTY")

    def test_unresolved_locator_refuses_to_compile(self) -> None:
        recording = minimal_recording()
        recording["steps"][0]["disambiguation"] = {"strategy": "unresolved"}
        with self.assertRaises(RecordingError) as caught:
            compile_recording(recording)
        self.assertEqual(caught.exception.code, "RECORDING.LOCATOR_UNRESOLVED")

    def test_ordinal_must_be_marked_fragile(self) -> None:
        recording = minimal_recording()
        recording["steps"][0]["disambiguation"] = {
            "strategy": "ordinal",
            "verified": True,
        }
        with self.assertRaises(RecordingError) as caught:
            compile_recording(recording)
        self.assertEqual(caught.exception.code, "RECORDING.LOCATOR_FRAGILE")

    def test_cross_platform_replay_is_refused(self) -> None:
        recording = minimal_recording()
        recording["platform_binding"]["replay_platforms"] = ["windows", "linux"]
        with self.assertRaises(RecordingError) as caught:
            compile_recording(recording)
        self.assertEqual(caught.exception.code, "RECORDING.PLATFORM_MISMATCH")

    def test_literal_text_must_be_disclosed(self) -> None:
        recording = minimal_recording()
        recording["steps"][0] = {
            "id": "typed",
            "kind": "interaction",
            "action": "type_text",
            "locator": {"role": "edit"},
            "disambiguation": {"strategy": "unique", "verified": True},
            "text": {"source": "literal", "value": "hunter2"},
        }
        with self.assertRaises(RecordingError) as caught:
            compile_recording(recording)
        self.assertEqual(caught.exception.code, "RECORDING.REDACTION_INVALID")

    def test_budget_is_derived_from_the_step_count(self) -> None:
        workflow = compile_recording(minimal_recording())
        # One interaction expands to three executed steps.
        self.assertGreaterEqual(workflow["budgets"]["max_executed_steps"], 3)

    def test_generated_workflow_passes_the_real_compiler(self) -> None:
        workflow = compile_recording(minimal_recording())
        compile_descriptor(workflow, source="test")


class TrackedExampleTests(unittest.TestCase):
    """The tracked example must actually work, not merely claim to."""

    def load(self) -> dict:
        import yaml

        return yaml.safe_load(EXAMPLE.read_text(encoding="utf-8"))

    def test_example_compiles_and_matches_its_committed_output(self) -> None:
        import json

        workflow = compile_recording(self.load())
        compile_descriptor(copy.deepcopy(workflow), source="example")

        committed = json.loads(
            (EXAMPLE.parent / "save-note.compiled.json").read_text(encoding="utf-8")
        )
        self.assertEqual(
            [s["id"] for s in workflow["steps"]],
            [s["id"] for s in committed["steps"]],
        )
        for key in ("apiVersion", "kind", "requires", "inputs"):
            self.assertEqual(workflow[key], committed[key], key)

    def test_example_attaches_its_assertion(self) -> None:
        workflow = compile_recording(self.load())
        ids = [s["id"] for s in workflow["steps"]]
        self.assertNotIn("note_present", ids)
        note = [s for s in workflow["steps"] if s["id"] == "enter_note"][0]
        self.assertIn("postcondition", note)


if __name__ == "__main__":
    unittest.main()
