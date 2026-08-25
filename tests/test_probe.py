"""Tests for conservative, read-only platform capability probes."""

from __future__ import annotations

from contextlib import redirect_stdout
import io
import json
import unittest
from unittest import mock

from ai_auto_desktop import cli
from ai_auto_desktop import probe


class ProbeTests(unittest.TestCase):
    def test_report_has_stable_json_shape_and_state_counts(self) -> None:
        checks = (
            probe.CapabilityCheck("linux.one", "available", "present", {"seen": True}),
            probe.CapabilityCheck("linux.two", "unknown", "uncertain"),
        )
        report = probe.ProbeReport({"name": "linux"}, {"kind": "x11"}, checks).to_dict()

        self.assertEqual(report["api_version"], "ai-auto-desktop.dev/probe/v1alpha1")
        self.assertEqual(report["kind"], "CapabilityProbe")
        self.assertEqual(report["status"], "completed")
        self.assertEqual(report["checks"]["linux.one"]["state"], "available")
        self.assertEqual(report["summary"], {"available": 1, "degraded": 0, "unavailable": 0, "unknown": 1})
        json.dumps(report)

    def test_platform_dispatch_uses_canonical_name(self) -> None:
        expected = probe.CapabilityCheck("windows.uia", "unknown", "fixture")
        with (
            mock.patch.object(probe.platform, "system", return_value="Windows"),
            mock.patch.object(probe.platform, "release", return_value="11"),
            mock.patch.object(probe.platform, "version", return_value="build"),
            mock.patch.object(probe.platform, "machine", return_value="AMD64"),
            mock.patch.object(probe.platform, "python_version", return_value="3.11.0"),
            mock.patch.object(probe, "_probe_windows", return_value=(expected,)) as windows,
        ):
            report = probe.probe_capabilities(environ={}).to_dict()

        windows.assert_called_once_with()
        self.assertEqual(report["platform"]["name"], "windows")
        self.assertIn("windows.uia", report["checks"])

    def test_macos_preflights_permissions_without_requesting_them(self) -> None:
        with mock.patch.object(
            probe, "_call_boolean_symbol", side_effect=(("ok", True), ("ok", False))
        ) as call:
            accessibility, capture = probe._probe_macos()

        self.assertEqual(accessibility.state, "available")
        self.assertEqual(capture.state, "unavailable")
        self.assertFalse(accessibility.evidence["prompt_requested"])
        self.assertFalse(capture.evidence["capture_attempted"])
        symbols = [item.args[1] for item in call.call_args_list]
        self.assertEqual(symbols, ["AXIsProcessTrusted", "CGPreflightScreenCaptureAccess"])
        self.assertTrue(all("Request" not in symbol for symbol in symbols))

    def test_atspi_distinguishes_missing_from_advertised_but_unverified(self) -> None:
        with (
            mock.patch.object(probe, "_which", return_value=None),
            mock.patch.object(probe, "_library_found", return_value=False),
        ):
            missing = probe._probe_linux_atspi({})
            advertised = probe._probe_linux_atspi({"AT_SPI_BUS_ADDRESS": "unix:path=hidden"})

        self.assertEqual(missing.state, "unavailable")
        self.assertEqual(advertised.state, "degraded")
        self.assertNotIn("hidden", json.dumps(advertised.to_dict()))

    def test_portal_timeout_is_unknown_and_does_not_create_a_session(self) -> None:
        with (
            mock.patch.object(probe, "_which", return_value="/usr/bin/gdbus"),
            mock.patch.object(probe, "_run_read_only", return_value=probe._CommandResult("timeout")) as run,
        ):
            result = probe._probe_linux_portal({"DBUS_SESSION_BUS_ADDRESS": "hidden"})

        self.assertEqual(result.state, "unknown")
        argv = run.call_args.args[0]
        self.assertIn("org.freedesktop.DBus.Properties.Get", argv)
        self.assertNotIn("CreateSession", argv)
        self.assertFalse(result.evidence["permission_requested"])
        self.assertEqual(
            run.call_args.kwargs["pass_environment"],
            ("DBUS_SESSION_BUS_ADDRESS",),
        )

    def test_read_only_commands_use_absolute_paths_and_minimal_environment(self) -> None:
        class Completed:
            pid = 1
            returncode = 0
            stdout = io.BytesIO(b"ok")

            def poll(self) -> int:
                return 0

            def wait(self, timeout: float | None = None) -> int:
                return 0

        completed = Completed()
        supplied = {
            "PATH": "/tmp/untrusted",
            "DBUS_SESSION_BUS_ADDRESS": "unix:path=/fixture",
            "SECRET": "must-not-leak",
        }
        with mock.patch.object(probe.subprocess, "Popen", return_value=completed) as popen:
            result = probe._run_read_only(
                ("/usr/bin/gdbus", "--version"),
                environ=supplied,
                pass_environment=("DBUS_SESSION_BUS_ADDRESS",),
            )

        self.assertEqual(result.outcome, "ok")
        child_environment = popen.call_args.kwargs["env"]
        self.assertEqual(child_environment["PATH"], probe._TRUSTED_COMMAND_PATH)
        self.assertEqual(
            child_environment["DBUS_SESSION_BUS_ADDRESS"],
            "unix:path=/fixture",
        )
        self.assertNotIn("SECRET", child_environment)

    def test_read_only_command_rejects_path_lookup(self) -> None:
        with mock.patch.object(probe.subprocess, "Popen") as popen:
            result = probe._run_read_only(("gdbus", "--version"))

        self.assertEqual(result.outcome, "error")
        popen.assert_not_called()

    def test_x11_probe_uses_bounded_root_property_query(self) -> None:
        environment = {"DISPLAY": ":10.0", "XAUTHORITY": "/tmp/auth"}
        with (
            mock.patch.object(probe, "_which", return_value="/usr/bin/xprop"),
            mock.patch.object(probe, "_library_found", return_value=True),
            mock.patch.object(
                probe, "_run_read_only",
                return_value=probe._CommandResult("ok", 0, "atom(WINDOW)"),
            ) as run,
        ):
            result = probe._probe_linux_x11(environment)

        self.assertEqual(result.state, "available")
        self.assertTrue(result.evidence["xprop_found"])
        self.assertEqual(
            run.call_args.args[0],
            ("/usr/bin/xprop", "-root", "_NET_SUPPORTING_WM_CHECK"),
        )
        self.assertEqual(
            run.call_args.kwargs["pass_environment"],
            ("DISPLAY", "XAUTHORITY"),
        )

    def test_session_probe_tolerates_missing_stdin(self) -> None:
        with mock.patch.object(probe.sys, "stdin", None):
            session = probe._session_info("linux", {})

        self.assertFalse(session["signals"]["stdin_is_tty"])

    def test_remote_display_is_not_claimed_interactive_from_environment_alone(self) -> None:
        session = probe._session_info(
            "linux",
            {"SSH_CONNECTION": "fixture", "DISPLAY": ":99"},
        )

        self.assertEqual(session["kind"], "ssh_x11")
        self.assertIsNone(session["interactive"])

    def test_wayland_and_uinput_checks_only_inspect_metadata(self) -> None:
        with mock.patch.object(probe, "_path_state", return_value="socket"):
            wayland = probe._probe_linux_wayland({"WAYLAND_DISPLAY": "wayland-0", "XDG_RUNTIME_DIR": "/run/user/fixture"})
        with (
            mock.patch.object(probe, "_uinput_device", return_value=("/dev/uinput", "character_device", True, False)),
            mock.patch.object(probe, "_library_found", return_value=True),
            mock.patch.object(probe, "_which", return_value=None),
        ):
            uinput = probe._probe_linux_uinput()

        self.assertEqual(wayland.state, "available")
        self.assertEqual(uinput.state, "degraded")
        self.assertFalse(uinput.evidence["device_opened"])

    def test_write_only_uinput_is_sufficient_for_injection_prerequisite(self) -> None:
        with (
            mock.patch.object(
                probe,
                "_uinput_device",
                return_value=("/dev/uinput", "character_device", False, True),
            ),
            mock.patch.object(probe, "_library_found", return_value=True),
            mock.patch.object(probe, "_which", return_value=None),
        ):
            result = probe._probe_linux_uinput()

        self.assertEqual(result.state, "available")
        self.assertFalse(result.evidence["device_opened"])

    def test_cli_probe_emits_one_json_document_and_succeeds_when_unavailable(self) -> None:
        report = probe.ProbeReport(
            {"name": "linux"}, {"kind": "unknown"},
            (probe.CapabilityCheck("linux.at_spi", "unavailable", "missing"),),
        )
        output = io.StringIO()
        with mock.patch.object(cli, "probe_capabilities", return_value=report), redirect_stdout(output):
            returncode = cli.main(["probe"])

        self.assertEqual(returncode, 0)
        self.assertEqual(len(output.getvalue().splitlines()), 1)
        payload = json.loads(output.getvalue())
        self.assertEqual(payload["status"], "completed")
        self.assertEqual(payload["checks"]["linux.at_spi"]["state"], "unavailable")


if __name__ == "__main__":
    unittest.main()
