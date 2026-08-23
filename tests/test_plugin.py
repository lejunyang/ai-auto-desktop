"""Integration tests for the subprocess-backed plugin host."""

from __future__ import annotations

from pathlib import Path
import sys
import time
import unittest
from unittest import mock

from ai_auto_desktop.plugin import PluginError, ProcessPlugin


PROJECT_ROOT = Path(__file__).resolve().parents[1]
FIXTURE_PLUGIN = PROJECT_ROOT / "plugins" / "fixture" / "fixture_plugin.py"


class ProcessPluginTests(unittest.TestCase):
    def make_plugin(
        self, *, proactive_manifest: bool = False, timeout: float = 2.0
    ) -> ProcessPlugin:
        command = [sys.executable, str(FIXTURE_PLUGIN)]
        if proactive_manifest:
            command.append("--manifest")
        plugin = ProcessPlugin(command, timeout=timeout, name="fixture")
        self.addCleanup(plugin.close)
        return plugin

    def test_start_performs_request_handshake_and_is_idempotent(self) -> None:
        plugin = self.make_plugin()

        manifest = plugin.start()

        self.assertTrue(plugin.started)
        self.assertFalse(plugin.closed)
        self.assertIsInstance(plugin.pid, int)
        self.assertEqual(manifest["kind"], "CapabilityManifest")
        self.assertEqual(manifest["metadata"]["name"], "fixture")
        self.assertIn("invoke", manifest["actions"])
        self.assertIs(plugin.start(), manifest)

    def test_invalid_manifest_is_rejected(self) -> None:
        code = (
            "import json,sys; "
            "print(json.dumps({'apiVersion':'wrong','kind':'NotAManifest'}), flush=True); "
            "sys.stdin.read()"
        )
        plugin = ProcessPlugin([sys.executable, "-c", code], timeout=1)
        self.addCleanup(plugin.close)

        with self.assertRaises(PluginError) as raised:
            plugin.start()

        self.assertEqual(raised.exception.code, "PLUGIN.HOST_PROTOCOL_ERROR")

    def test_context_manager_accepts_proactive_handshake_and_closes(self) -> None:
        plugin = self.make_plugin(proactive_manifest=True)

        with plugin as entered:
            self.assertIs(entered, plugin)
            self.assertTrue(plugin.started)
            self.assertEqual(plugin.manifest["metadata"]["name"], "fixture")
            process = plugin._process
            self.assertIsNotNone(process)
            self.assertIsNone(process.poll())

        self.assertTrue(plugin.closed)
        self.assertIsNotNone(process.poll())

    def test_invoke_returns_fixture_result(self) -> None:
        plugin = self.make_plugin()
        args = {
            "operation": "click",
            "target": {"role": "button", "name": "Save"},
        }

        result = plugin.invoke("invoke", args)

        self.assertEqual(
            result,
            {
                "ok": True,
                "invoked": True,
                "operation": "click",
                "target": {"role": "button", "name": "Save"},
                "args": args,
            },
        )

    def test_structured_plugin_error_preserves_data_and_retryability(self) -> None:
        plugin = self.make_plugin()

        with self.assertRaises(PluginError) as raised:
            plugin.invoke(
                "error",
                {
                    "code": "FIXTURE.BUSY",
                    "message": "fixture is temporarily busy",
                    "retryable": True,
                    "data": {"retryAfterMs": 25},
                },
            )

        error = raised.exception
        self.assertEqual(error.code, "FIXTURE.BUSY")
        self.assertEqual(error.message, "fixture is temporarily busy")
        self.assertTrue(error.retryable)
        self.assertTrue(error.dispatched)
        self.assertEqual(error.details["retryAfterMs"], 25)
        self.assertEqual(
            error.to_dict(),
            {
                "code": "FIXTURE.BUSY",
                "message": "fixture is temporarily busy",
                "details": {"retryAfterMs": 25, "dispatched": True},
                "retryable": True,
            },
        )
        self.assertFalse(plugin.closed)

    def test_retryable_error_does_not_poison_the_plugin_session(self) -> None:
        plugin = self.make_plugin()
        args = {"key": "retry-test", "failures": 1}

        with self.assertRaises(PluginError) as raised:
            plugin.invoke("transient", args)

        error = raised.exception
        self.assertEqual(error.code, "FIXTURE.TRANSIENT")
        self.assertTrue(error.retryable)
        self.assertTrue(error.dispatched)
        self.assertEqual(error.details["attempt"], 1)
        self.assertEqual(
            plugin.invoke("transient", args),
            {
                "ok": True,
                "key": "retry-test",
                "attempt": 2,
                "failures": 1,
            },
        )

    def test_timeout_is_retryable_and_records_that_request_was_dispatched(self) -> None:
        plugin = self.make_plugin()
        plugin.start()

        with self.assertRaises(PluginError) as raised:
            plugin.invoke("sleep", {"milliseconds": 500}, timeout=0.05)

        error = raised.exception
        self.assertEqual(error.code, "PLUGIN.HOST_TIMEOUT")
        self.assertTrue(error.retryable)
        self.assertTrue(error.dispatched)
        self.assertIs(error.details["dispatched"], True)
        self.assertTrue(plugin.closed)

    def test_invoke_sends_an_absolute_deadline(self) -> None:
        plugin = self.make_plugin()
        plugin.start()
        invoke_timeout = 0.75

        before = time.time()
        with mock.patch.object(
            plugin, "_write_request", wraps=plugin._write_request
        ) as write_request:
            result = plugin.invoke(
                "ocr", {"result": {"text": "captured"}}, timeout=invoke_timeout
            )
        after = time.time()

        self.assertEqual(result, {"text": "captured"})
        write_request.assert_called_once()
        request = write_request.call_args.args[0]
        self.assertEqual(request["type"], "invoke")
        self.assertEqual(request["action"], "ocr")
        self.assertIsInstance(request["id"], str)
        self.assertGreaterEqual(
            request["deadline_ms"], int((before + invoke_timeout) * 1000) - 1
        )
        self.assertLessEqual(
            request["deadline_ms"], int((after + invoke_timeout) * 1000) + 1
        )

    def test_close_is_idempotent_and_prevents_restart(self) -> None:
        plugin = self.make_plugin()
        plugin.start()

        plugin.close()
        plugin.close()

        self.assertTrue(plugin.closed)
        with self.assertRaises(PluginError) as raised:
            plugin.start()
        self.assertEqual(raised.exception.code, "PLUGIN.CLOSED")
        self.assertFalse(raised.exception.dispatched)

    def test_invalid_action_is_rejected_without_starting_the_process(self) -> None:
        plugin = self.make_plugin()

        with self.assertRaises(PluginError) as raised:
            plugin.invoke("", {})

        self.assertEqual(raised.exception.code, "PLUGIN.INVALID_REQUEST")
        self.assertFalse(raised.exception.dispatched)
        self.assertIsNone(plugin.pid)

    def test_invalid_commands_are_rejected(self) -> None:
        for command in ([], [""], [sys.executable, ""]):
            with self.subTest(command=command):
                with self.assertRaises(ValueError):
                    ProcessPlugin(command)

        missing = PROJECT_ROOT / "does-not-exist" / "fixture-plugin"
        plugin = ProcessPlugin([str(missing)], name="missing fixture")
        self.addCleanup(plugin.close)
        with self.assertRaises(PluginError) as raised:
            plugin.start()
        self.assertEqual(raised.exception.code, "PLUGIN.START_FAILED")
        self.assertTrue(raised.exception.retryable)
        self.assertTrue(plugin.closed)


if __name__ == "__main__":
    unittest.main()
