"""Tests for the recording editor core and its HTTP surface.

The server tests bind a real loopback socket rather than calling handlers
directly: the auth boundary, the status codes and the JSON contract are the
things worth pinning, and none of them are exercised by invoking the functions
underneath.
"""

from __future__ import annotations

import json
import socket
import threading
import unittest
import urllib.error
import urllib.request

from ai_auto_desktop import recording_editor as editor
from ai_auto_desktop.editor_page import PAGE
from ai_auto_desktop.editor_server import TOKEN_PLACEHOLDER, serve
from ai_auto_desktop.recording import RecordingError


def sample_recording() -> dict:
    return {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "Recording",
        "metadata": {
            "name": "demo",
            "version": "0.1.0",
            "annotations": {
                editor.RECORDED_ORDER_KEY: "open_dialog,dialog_visible,click_save"
            },
        },
        "capture": {
            "platform": "windows",
            "recorded_at": "2026-08-30T00:00:00Z",
            "driver": {"name": "desktop.windows_uia", "version": "0.1.0"},
            "window": {"class_name": "Notepad"},
        },
        "inputs": {"should_save": {"type": "string"}},
        "redaction": {
            "value_policy": "drop",
            "title_policy": "drop",
            "screenshots": "none",
            "disclosed": [],
        },
        "platform_binding": {"platform": "windows",
                             "replay_platforms": ["windows"]},
        "steps": [
            {"id": "open_dialog", "kind": "interaction", "action": "invoke",
             "locator": {"role": "button", "name": "Open"},
             "disambiguation": {"strategy": "unique", "verified": True}},
            {"id": "dialog_visible", "kind": "assertion",
             "of_step": "open_dialog",
             "observe": {"action": "find", "locator": {"role": "window"}},
             "expect": {"mode": "exists"}},
            {"id": "click_save", "kind": "interaction", "action": "invoke",
             "locator": {"role": "button", "name": "Save"},
             "disambiguation": {"strategy": "unique", "verified": True}},
        ],
    }


class EditingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.recording = sample_recording()

    def test_editing_a_locator_clears_its_verification(self) -> None:
        # Otherwise an edited locator inherits proof it never earned, which is
        # exactly what verify_locators exists to prevent.
        updated = editor.update_step(
            self.recording, "click_save",
            {"locator": {"role": "button", "name": "Store"}})
        step = next(s for s in updated["steps"] if s["id"] == "click_save")
        self.assertEqual(step["disambiguation"],
                         {"strategy": "unresolved", "verified": False})

    def test_edits_do_not_mutate_the_input(self) -> None:
        before = json.dumps(self.recording, sort_keys=True)
        editor.update_step(self.recording, "click_save",
                           {"description": "changed"})
        self.assertEqual(json.dumps(self.recording, sort_keys=True), before)

    def test_protected_fields_are_refused(self) -> None:
        with self.assertRaises(RecordingError) as caught:
            editor.update_step(self.recording, "click_save",
                               {"action": "type_text"})
        self.assertIn("action", caught.exception.details["fields"])

    def test_unknown_fields_are_refused(self) -> None:
        # An allow-list, so a field added to the format later is not silently
        # editable just because nobody remembered to protect it.
        with self.assertRaises(RecordingError):
            editor.update_step(self.recording, "click_save", {"whatever": 1})

    def test_editing_a_nested_step(self) -> None:
        wrapped = editor.insert_logic(self.recording, "guard",
                                      "${{ inputs.should_save }}",
                                      wrap=["click_save"])
        updated = editor.update_step(wrapped, "click_save",
                                     {"description": "inner"})
        guard = next(s for s in updated["steps"] if s["id"] == "guard")
        self.assertEqual(guard["steps"][0]["description"], "inner")

    def test_reorder_requires_a_full_permutation(self) -> None:
        # A partial list would silently drop whatever was omitted.
        with self.assertRaises(RecordingError):
            editor.reorder(self.recording, ["click_save"])

    def test_reorder_warns_instead_of_refusing(self) -> None:
        # Measured: moving a step ahead of the interaction that established its
        # precondition compiles cleanly, so no static check can catch it.
        # Refusing would block deliberate restructuring; silence would hide it.
        updated, warnings = editor.reorder(
            self.recording, ["click_save", "open_dialog", "dialog_visible"])
        self.assertEqual([s["id"] for s in updated["steps"]],
                         ["click_save", "open_dialog", "dialog_visible"])
        self.assertIn("ORDER_DIVERGED", [w["code"] for w in warnings])

    def test_assertion_before_its_target_is_reported(self) -> None:
        _, warnings = editor.reorder(
            self.recording, ["dialog_visible", "open_dialog", "click_save"])
        self.assertIn("ASSERTION_BEFORE_TARGET", [w["code"] for w in warnings])

    def test_wrapping_keeps_the_flow_position(self) -> None:
        wrapped = editor.insert_logic(self.recording, "guard",
                                      "${{ inputs.should_save }}",
                                      wrap=["click_save"])
        self.assertEqual([s["id"] for s in wrapped["steps"]],
                         ["open_dialog", "dialog_visible", "guard"])
        guard = wrapped["steps"][2]
        self.assertEqual([s["id"] for s in guard["steps"]], ["click_save"])

    def test_duplicate_step_ids_are_refused(self) -> None:
        with self.assertRaises(RecordingError):
            editor.insert_logic(self.recording, "click_save", "true",
                                wrap=["click_save"])

    def test_a_recording_that_already_has_duplicate_ids_is_refused(self) -> None:
        # Mutation testing found the structural duplicate check untested:
        # insert_logic has its own guard, so the id rule in _validated was only
        # ever reached by a path that never got there.  A duplicate arriving in
        # the loaded file has to be caught too, because ids are how every
        # assertion and logic branch refers to a step.
        broken = sample_recording()
        broken["steps"].append(dict(broken["steps"][0]))
        with self.assertRaises(RecordingError) as caught:
            editor.set_enabled(broken, "click_save", False)
        self.assertEqual(caught.exception.details["id"], "open_dialog")

    def test_dangling_assertion_is_refused(self) -> None:
        # The one integrity rule measured NOT to be caught by the workflow
        # compiler, so the editor has to enforce it.
        broken = sample_recording()
        broken["steps"][1]["of_step"] = "no_such_step"
        with self.assertRaises(RecordingError):
            editor.set_enabled(broken, "click_save", False)

    def test_an_unfinished_recording_can_still_be_saved(self) -> None:
        # Editing a locator makes a recording uncompilable; if saving required
        # compilability, "fix a locator then re-verify it" would be impossible.
        edited = editor.update_step(
            self.recording, "click_save",
            {"locator": {"role": "button", "name": "Store"}})
        again = editor.update_step(edited, "click_save",
                                   {"description": "still editable"})
        self.assertFalse(editor.compile_preview(again)["ok"])

    def test_compile_preview_reports_instead_of_raising(self) -> None:
        broken = editor.update_step(
            self.recording, "click_save",
            {"locator": {"role": "button", "name": "Store"}})
        preview = editor.compile_preview(broken)
        self.assertFalse(preview["ok"])
        self.assertEqual(preview["error"]["code"],
                         "RECORDING.LOCATOR_UNRESOLVED")
        self.assertEqual(preview["error"]["details"]["id"], "click_save")

    def test_view_exposes_disambiguation_for_every_interaction(self) -> None:
        # The driver fails closed on ambiguity, so the operator must see this
        # while editing rather than as DRIVER.AMBIGUOUS during replay.
        rows = editor.step_view(self.recording)
        interactions = [r for r in rows if r["kind"] == "interaction"]
        self.assertTrue(all(r["strategy"] for r in interactions))

    def test_view_keeps_nesting_visible(self) -> None:
        wrapped = editor.insert_logic(self.recording, "guard",
                                      "${{ inputs.should_save }}",
                                      wrap=["click_save"])
        rows = editor.step_view(wrapped)
        nested = next(r for r in rows if r["id"] == "click_save")
        self.assertEqual(nested["depth"], 1)
        self.assertEqual(nested["parent"], "guard")


class ServerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.server = serve(sample_recording(), PAGE)
        self.thread = threading.Thread(target=self.server.serve_forever,
                                       daemon=True)
        self.thread.start()
        self.base = self.server.url.rstrip("/")
        self.token = self.server.token
        self.addCleanup(self._stop)

    def _stop(self) -> None:
        self.server.shutdown()
        self.server.server_close()

    def call(self, path, body=None, token="__default__"):
        if token == "__default__":
            token = self.token
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(
            self.base + path, data=data, method="POST" if data else "GET")
        if token:
            request.add_header("X-Recorder-Token", token)
        if data:
            request.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                raw = response.read().decode("utf-8")
                return response.status, raw
        except urllib.error.HTTPError as exc:
            return exc.code, exc.read().decode("utf-8")

    def json_call(self, path, body=None, token="__default__"):
        status, raw = self.call(path, body, token)
        return status, json.loads(raw)

    def test_binds_loopback_on_an_os_assigned_port(self) -> None:
        host, port = self.server.server_address[:2]
        self.assertEqual(host, "127.0.0.1")
        self.assertGreater(port, 0)

    def test_api_requires_the_token(self) -> None:
        self.assertEqual(self.json_call("/api/state", token=None)[0], 401)
        self.assertEqual(self.json_call("/api/state", token="wrong")[0], 401)
        self.assertEqual(self.json_call("/api/state")[0], 200)

    def test_mutating_endpoints_require_the_token(self) -> None:
        status, _ = self.json_call(
            "/api/enable", {"id": "click_save", "enabled": False}, token=None)
        self.assertEqual(status, 401)

    def test_no_unauthenticated_endpoint_serves_the_token(self) -> None:
        # Measured: an unauthenticated /bootstrap let a separate local process
        # take the token and read the session in one step.
        self.assertEqual(self.json_call("/bootstrap", token=None)[0], 401)

    def test_the_page_carries_the_token_and_no_placeholder(self) -> None:
        status, html = self.call("/", token=None)
        self.assertEqual(status, 200)
        self.assertNotIn(TOKEN_PLACEHOLDER, html)
        self.assertIn(self.token, html)

    def test_unknown_endpoints_are_not_found(self) -> None:
        self.assertEqual(self.json_call("/api/anything", {})[0], 404)

    def test_there_is_no_generic_execute_endpoint(self) -> None:
        # The UI must not be able to run arbitrary driver actions: that would
        # make it a way around policy and risk checks.
        # Sent with a body, which is what caught the early-rejection defect:
        # responding before draining made the peer see a reset, not a 404.
        for path in ("/api/execute", "/api/run", "/api/action", "/api/eval"):
            self.assertEqual(self.json_call(path, {"payload": "x" * 4096})[0],
                             404, path)

    def test_state_reports_steps_warnings_and_compilability(self) -> None:
        _, state = self.json_call("/api/state")
        self.assertEqual([s["id"] for s in state["steps"]],
                         ["open_dialog", "dialog_visible", "click_save"])
        self.assertTrue(state["compile"]["ok"])
        self.assertIn("desktop.input", state["compile"]["permissions"])

    def test_edits_round_trip(self) -> None:
        _, state = self.json_call(
            "/api/update",
            {"id": "click_save",
             "changes": {"locator": {"role": "button", "name": "Store"}}})
        step = next(s for s in state["steps"] if s["id"] == "click_save")
        self.assertEqual(step["strategy"], "unresolved")
        self.assertFalse(state["compile"]["ok"])

    def test_undo_restores_the_previous_state(self) -> None:
        self.json_call("/api/update",
                       {"id": "click_save",
                        "changes": {"locator": {"role": "button",
                                                "name": "Store"}}})
        _, state = self.json_call("/api/undo", {})
        step = next(s for s in state["steps"] if s["id"] == "click_save")
        self.assertEqual(step["locator"], {"role": "button", "name": "Save"})
        self.assertTrue(state["compile"]["ok"])

    def test_a_refused_edit_returns_conflict_and_changes_nothing(self) -> None:
        status, payload = self.json_call(
            "/api/update", {"id": "click_save", "changes": {"action": "x"}})
        self.assertEqual(status, 409)
        self.assertEqual(payload["error"]["code"], "RECORDING.ORDER_INVALID")
        _, state = self.json_call("/api/state")
        step = next(s for s in state["steps"] if s["id"] == "click_save")
        self.assertEqual(step["action"], "invoke")

    def test_reorder_returns_warnings(self) -> None:
        _, state = self.json_call(
            "/api/reorder",
            {"order": ["click_save", "open_dialog", "dialog_visible"]})
        self.assertIn("ORDER_DIVERGED", [w["code"] for w in state["warnings"]])

    def test_reorder_refuses_nested_step_ids(self) -> None:
        # /api/state returns a flattened list including logic children, so a
        # caller can easily pass an id that is not a top-level step.  Reordering
        # is defined over the top level only; accepting a child id would have to
        # mean moving it out of its branch, which changes what it guards.
        wrapped = editor.insert_logic(sample_recording(), "guard",
                                      "${{ inputs.should_save }}",
                                      wrap=["click_save"])
        server = serve(wrapped, PAGE)
        self.addCleanup(server.server_close)
        with self.assertRaises(RecordingError):
            server.session.apply(
                lambda rec: editor.reorder(
                    rec, ["click_save", "open_dialog", "dialog_visible"]))

    def test_malformed_json_is_a_bad_request(self) -> None:
        request = urllib.request.Request(
            self.base + "/api/enable", data=b"{not json",
            method="POST")
        request.add_header("X-Recorder-Token", self.token)
        request.add_header("Content-Type", "application/json")
        try:
            urllib.request.urlopen(request, timeout=10)
            self.fail("malformed body was accepted")
        except urllib.error.HTTPError as exc:
            self.assertEqual(exc.code, 400)

    def test_non_loopback_interfaces_cannot_reach_the_port(self) -> None:
        # The property the whole "safe to run a local server" argument rests on.
        try:
            lan = socket.gethostbyname(socket.gethostname())
        except socket.gaierror:
            self.skipTest("no resolvable host address")
        if lan.startswith("127."):
            self.skipTest("host resolves to loopback")
        probe = socket.socket()
        probe.settimeout(4)
        try:
            probe.connect((lan, self.server.server_address[1]))
            self.fail("the editor port is reachable from a non-loopback address")
        except OSError:
            pass
        finally:
            probe.close()


class AutosaveTests(unittest.TestCase):
    """The session must survive an exit path that never unwinds."""

    def test_every_edit_is_reported(self) -> None:
        # Measured on Windows: a killed process runs no finally block and gets
        # no KeyboardInterrupt, so saving only at exit can lose the session.
        saved = []
        server = serve(sample_recording(), PAGE, on_change=saved.append)
        self.addCleanup(server.server_close)
        server.session.apply(
            lambda rec: editor.set_enabled(rec, "click_save", False))
        self.assertEqual(len(saved), 1)
        step = next(s for s in saved[-1]["steps"] if s["id"] == "click_save")
        self.assertFalse(step["enabled"])

    def test_undo_is_reported_too(self) -> None:
        # Without this the saved file keeps an edit the operator took back.
        saved = []
        server = serve(sample_recording(), PAGE, on_change=saved.append)
        self.addCleanup(server.server_close)
        server.session.apply(
            lambda rec: editor.set_enabled(rec, "click_save", False))
        server.session.undo()
        self.assertEqual(len(saved), 2)
        step = next(s for s in saved[-1]["steps"] if s["id"] == "click_save")
        self.assertNotIn("enabled", step)

    def test_a_failing_save_does_not_roll_back_the_edit(self) -> None:
        # The screen and the file would otherwise disagree about the recording.
        def explode(_recording):
            raise OSError("disk full")

        server = serve(sample_recording(), PAGE, on_change=explode)
        self.addCleanup(server.server_close)
        server.session.apply(
            lambda rec: editor.set_enabled(rec, "click_save", False))
        step = next(s for s in server.session.recording["steps"]
                    if s["id"] == "click_save")
        self.assertFalse(step["enabled"])


if __name__ == "__main__":
    unittest.main()
