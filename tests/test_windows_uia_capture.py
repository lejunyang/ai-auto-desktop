"""Contract tests for the Windows UIA capture (record) actions.

These exercise the platform-independent capture layer through an injected fake
backend, so they run on any host.  The native COM path is covered separately by
the on-device fixture run.

The behaviours pinned here are the ones that were measured, and in several cases
the ones an earlier version of this work got wrong:

* value text must never appear in a captured event, even though it is readable;
* dropped events must be reported, because a silent drop is indistinguishable
  from the user doing nothing;
* known blind spots must be announced at capture_start rather than discovered
  as silence;
* callbacks arrive on foreign threads, so the buffer must be safe under
  concurrent emit and drain.
"""

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import threading
import time
import unittest


PROJECT_ROOT = Path(__file__).resolve().parents[1]
DRIVER_PATH = PROJECT_ROOT / "plugins" / "windows_uia" / "windows_uia_driver.py"

SPEC = importlib.util.spec_from_file_location("capture_windows_uia_driver", DRIVER_PATH)
assert SPEC is not None and SPEC.loader is not None
uia = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = uia
SPEC.loader.exec_module(uia)


def deadline() -> float:
    return time.monotonic() + 5.0


class FakeCaptureBackend:
    """Backend that records subscription lifecycle and lets tests emit events."""

    name = "fake_capture"

    def __init__(self, *, fail_unsubscribe: bool = False) -> None:
        self.subscriptions: list[dict] = []
        self.removed: list[dict] = []
        self.sink: uia.CaptureSink | None = None
        self.fail_unsubscribe = fail_unsubscribe

    def subscribe(self, window, sink, *, deadline):
        self.sink = sink
        subscription = {"window": dict(window), "sink": sink}
        self.subscriptions.append(subscription)
        return subscription

    def unsubscribe(self, subscription, *, deadline):
        if self.fail_unsubscribe:
            raise RuntimeError("native teardown exploded")
        self.removed.append(subscription)


class BackendWithoutCapture:
    """A backend predating capture support."""

    name = "no_capture"


class CaptureSinkTests(unittest.TestCase):
    def test_sequence_numbers_are_monotonic(self) -> None:
        sink = uia.CaptureSink()
        for _ in range(5):
            sink.emit("focus_changed", {"name": "x"})
        events, _ = sink.drain(10)
        self.assertEqual([e["sequence"] for e in events], [0, 1, 2, 3, 4])

    def test_value_is_never_recorded(self) -> None:
        # The element mapping carries a value; the captured record must not.
        sink = uia.CaptureSink()
        sink.emit("value_changed", {"name": "Field", "value": "hunter2"})
        events, _ = sink.drain(10)
        self.assertNotIn("value", events[0]["element"])
        self.assertNotIn("hunter2", repr(events[0]))

    def test_overflow_is_counted_not_hidden(self) -> None:
        sink = uia.CaptureSink(limit=3)
        for index in range(10):
            sink.emit("invoked", {"name": str(index)})
        events, dropped = sink.drain(10)
        self.assertEqual(len(events), 3)
        self.assertEqual(dropped, 7)
        # The survivors are the newest, and the loss is reported exactly once.
        self.assertEqual([e["element"]["name"] for e in events], ["7", "8", "9"])
        _, again = sink.drain(10)
        self.assertEqual(again, 0)

    def test_emit_survives_a_hostile_element(self) -> None:
        # A native element can die mid-callback; that must not kill the session.
        class Exploding(dict):
            def get(self, *args, **kwargs):
                raise RuntimeError("element vanished")

        sink = uia.CaptureSink()
        sink.emit("invoked", Exploding())
        events, _ = sink.drain(10)
        self.assertEqual(len(events), 1)
        self.assertIsNone(events[0]["element"]["name"])

    def test_concurrent_emit_and_drain_lose_nothing(self) -> None:
        # Callbacks arrive on COM RPC threads while requests drain the buffer.
        sink = uia.CaptureSink(limit=10_000)
        collected: list[dict] = []
        stop = threading.Event()

        def producer(base: int) -> None:
            for index in range(500):
                sink.emit("invoked", {"name": f"{base}-{index}"})

        def consumer() -> None:
            while not stop.is_set():
                events, _ = sink.drain(50)
                collected.extend(events)

        reader = threading.Thread(target=consumer)
        reader.start()
        writers = [threading.Thread(target=producer, args=(n,)) for n in range(4)]
        for writer in writers:
            writer.start()
        for writer in writers:
            writer.join()
        time.sleep(0.2)
        stop.set()
        reader.join()
        remaining, dropped = sink.drain(10_000)
        collected.extend(remaining)
        self.assertEqual(dropped, 0)
        self.assertEqual(len(collected), 2000)
        self.assertEqual(len({e["sequence"] for e in collected}), 2000)


class CaptureActionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.backend = FakeCaptureBackend()
        self.driver = uia.WindowsUIADriver(self.backend)

    def start(self) -> str:
        started = self.driver.execute(
            "capture_start", {"window": {"title": "Demo"}}, deadline=deadline()
        )
        return started["session_id"]

    def test_start_announces_blind_spots(self) -> None:
        started = self.driver.execute(
            "capture_start", {"window": {"title": "Demo"}}, deadline=deadline()
        )
        kinds = {spot["kind"] for spot in started["blind_spots"]}
        # Measured: neither raises any event, so the UI must be told up front.
        self.assertEqual(kinds, {"non_focusable_click", "pointer_motion"})

    def test_poll_returns_emitted_events(self) -> None:
        session_id = self.start()
        self.backend.sink.emit("invoked", {"name": "Save", "class_name": "Button"})
        result = self.driver.execute(
            "capture_poll", {"session_id": session_id}, deadline=deadline()
        )
        self.assertEqual(len(result["events"]), 1)
        self.assertEqual(result["events"][0]["kind"], "invoked")
        self.assertEqual(result["events"][0]["element"]["name"], "Save")
        self.assertTrue(result["active"])

    def test_poll_drains_so_events_are_not_replayed(self) -> None:
        session_id = self.start()
        self.backend.sink.emit("invoked", {"name": "Save"})
        self.driver.execute(
            "capture_poll", {"session_id": session_id}, deadline=deadline()
        )
        second = self.driver.execute(
            "capture_poll", {"session_id": session_id}, deadline=deadline()
        )
        self.assertEqual(second["events"], [])

    def test_poll_reports_dropped_events(self) -> None:
        session_id = self.start()
        for index in range(uia.MAX_CAPTURE_EVENTS + 5):
            self.backend.sink.emit("invoked", {"name": str(index)})
        result = self.driver.execute(
            "capture_poll",
            {"session_id": session_id, "max_events": 10},
            deadline=deadline(),
        )
        self.assertEqual(result["dropped_events"], 5)

    def test_poll_honours_max_events(self) -> None:
        session_id = self.start()
        for index in range(20):
            self.backend.sink.emit("invoked", {"name": str(index)})
        result = self.driver.execute(
            "capture_poll",
            {"session_id": session_id, "max_events": 5},
            deadline=deadline(),
        )
        self.assertEqual(len(result["events"]), 5)

    def test_poll_rejects_out_of_range_max_events(self) -> None:
        session_id = self.start()
        with self.assertRaises(uia.DriverError) as caught:
            self.driver.execute(
                "capture_poll",
                {"session_id": session_id, "max_events": 0},
                deadline=deadline(),
            )
        self.assertEqual(caught.exception.code, "DRIVER.INVALID_REQUEST")

    def test_stop_releases_the_native_subscription(self) -> None:
        session_id = self.start()
        self.driver.execute(
            "capture_stop", {"session_id": session_id}, deadline=deadline()
        )
        self.assertEqual(len(self.backend.removed), 1)

    def test_stop_reports_events_that_were_never_delivered(self) -> None:
        session_id = self.start()
        self.backend.sink.emit("invoked", {"name": "unread"})
        stopped = self.driver.execute(
            "capture_stop", {"session_id": session_id}, deadline=deadline()
        )
        # Stopping with unread events must not pretend nothing happened.
        self.assertEqual(stopped["dropped_events"], 1)

    def test_polling_a_stopped_session_fails(self) -> None:
        session_id = self.start()
        self.driver.execute(
            "capture_stop", {"session_id": session_id}, deadline=deadline()
        )
        with self.assertRaises(uia.DriverError) as caught:
            self.driver.execute(
                "capture_poll", {"session_id": session_id}, deadline=deadline()
            )
        self.assertEqual(caught.exception.code, "DRIVER.CAPTURE_NOT_FOUND")

    def test_unknown_session_is_rejected(self) -> None:
        with self.assertRaises(uia.DriverError) as caught:
            self.driver.execute(
                "capture_poll", {"session_id": "nope"}, deadline=deadline()
            )
        self.assertEqual(caught.exception.code, "DRIVER.CAPTURE_NOT_FOUND")

    def test_backend_without_capture_is_reported(self) -> None:
        driver = uia.WindowsUIADriver(BackendWithoutCapture())
        with self.assertRaises(uia.DriverError) as caught:
            driver.execute(
                "capture_start", {"window": {"title": "Demo"}}, deadline=deadline()
            )
        self.assertEqual(caught.exception.code, "DRIVER.CAPTURE_UNSUPPORTED")

    def test_concurrent_sessions_are_bounded(self) -> None:
        for _ in range(uia.MAX_CAPTURE_SESSIONS):
            self.start()
        with self.assertRaises(uia.DriverError) as caught:
            self.start()
        self.assertEqual(caught.exception.code, "DRIVER.CAPTURE_LIMIT")

    def test_sessions_are_isolated(self) -> None:
        first = self.start()
        first_sink = self.backend.sink
        second = self.start()
        second_sink = self.backend.sink
        first_sink.emit("invoked", {"name": "first"})
        second_sink.emit("invoked", {"name": "second"})
        first_result = self.driver.execute(
            "capture_poll", {"session_id": first}, deadline=deadline()
        )
        second_result = self.driver.execute(
            "capture_poll", {"session_id": second}, deadline=deadline()
        )
        self.assertEqual(first_result["events"][0]["element"]["name"], "first")
        self.assertEqual(second_result["events"][0]["element"]["name"], "second")

    def test_failing_teardown_surfaces_as_a_driver_error(self) -> None:
        backend = FakeCaptureBackend(fail_unsubscribe=True)
        driver = uia.WindowsUIADriver(backend)
        started = driver.execute(
            "capture_start", {"window": {"title": "Demo"}}, deadline=deadline()
        )
        with self.assertRaises(uia.DriverError) as caught:
            driver.execute(
                "capture_stop",
                {"session_id": started["session_id"]},
                deadline=deadline(),
            )
        self.assertEqual(caught.exception.code, "DRIVER.ACTION_FAILED")

    def test_empty_window_selector_is_rejected(self) -> None:
        with self.assertRaises(uia.DriverError) as caught:
            self.driver.execute("capture_start", {"window": {}}, deadline=deadline())
        self.assertEqual(caught.exception.code, "DRIVER.INVALID_REQUEST")


class CaptureContractTests(unittest.TestCase):
    def test_actions_are_declared_read_only_in_effect_on_the_ui(self) -> None:
        for name in ("capture_start", "capture_poll", "capture_stop"):
            contract = uia.ACTION_CONTRACTS[name]
            self.assertEqual(contract["risk"]["category"], "observe")
            # Capture observes; it must never be granted input injection.
            self.assertEqual(tuple(contract["permissions"]), ("desktop.observe",))

    def test_event_schema_has_no_value_field(self) -> None:
        properties = uia.CAPTURE_EVENT_SCHEMA["properties"]["element"]["properties"]
        self.assertNotIn("value", properties)
        self.assertNotIn("text", properties)

    def test_event_kind_mapping_matches_measured_ids(self) -> None:
        self.assertEqual(uia._CAPTURE_EVENT_KINDS[20009], "invoked")
        self.assertEqual(uia._CAPTURE_EVENT_KINDS[20015], "value_changed")
        self.assertEqual(uia._CAPTURE_EVENT_KINDS[20012], "selection_changed")
        self.assertEqual(uia.UIA_VALUE_PROPERTY_ID, 30045)

    def test_manifest_advertises_capture(self) -> None:
        actions = uia.MANIFEST["actions"]
        for name in ("capture_start", "capture_poll", "capture_stop"):
            self.assertIn(name, actions)


if __name__ == "__main__":
    unittest.main()
