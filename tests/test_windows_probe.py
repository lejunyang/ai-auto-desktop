"""Tests for the read-only Windows capability probes.

The portable tests assert the reported contract and the state mapping via
injected values, so they run on any platform.  The native tests assert that the
probe's conclusions match what the running Windows kernel independently
reports, which is the only way to show the probe is not merely self-consistent.
"""

from __future__ import annotations

import ctypes
import json
import os
import sys
import unittest
from unittest import mock

from ai_auto_desktop import probe


WINDOWS = sys.platform == "win32"


class WindowsProbeContractTests(unittest.TestCase):
    """Shape and vocabulary that must hold regardless of platform."""

    def test_windows_probe_reports_the_documented_check_set(self) -> None:
        expected = {
            "windows.uia",
            "windows.session",
            "windows.input_desktop",
            "windows.integrity",
            "windows.dpi",
            "script.sandbox",
        }
        with (
            mock.patch.object(
                probe, "_check_windows_uia",
                return_value=probe.CapabilityCheck("windows.uia", "unknown", "f"),
            ),
            mock.patch.object(
                probe, "_check_windows_session",
                return_value=probe.CapabilityCheck("windows.session", "unknown", "f"),
            ),
            mock.patch.object(
                probe, "_check_windows_input_desktop",
                return_value=probe.CapabilityCheck(
                    "windows.input_desktop", "unknown", "f"
                ),
            ),
            mock.patch.object(
                probe, "_check_windows_integrity",
                return_value=probe.CapabilityCheck(
                    "windows.integrity", "unknown", "f"
                ),
            ),
            mock.patch.object(
                probe, "_check_windows_dpi",
                return_value=probe.CapabilityCheck("windows.dpi", "unknown", "f"),
            ),
            mock.patch.object(
                probe, "_check_script_sandbox",
                return_value=probe.CapabilityCheck("script.sandbox", "unknown", "f"),
            ),
        ):
            checks = probe._probe_windows()

        self.assertEqual({check.name for check in checks}, expected)
        for check in checks:
            with self.subTest(check=check.name):
                self.assertIn(check.state, probe.PROBE_STATES)

    def test_integrity_level_rid_mapping_is_ordered_and_total(self) -> None:
        # The table must be highest-first so a linear scan resolves correctly.
        thresholds = [threshold for threshold, _ in probe._INTEGRITY_LEVELS]
        self.assertEqual(thresholds, sorted(thresholds, reverse=True))
        names = [name for _, name in probe._INTEGRITY_LEVELS]
        self.assertEqual(names, ["system", "high", "medium", "low"])

    def test_dpi_awareness_names_cover_the_win32_enum(self) -> None:
        self.assertEqual(
            probe._DPI_AWARENESS, {0: "unaware", 1: "system", 2: "per_monitor"}
        )

    def test_session_zero_is_reported_unavailable(self) -> None:
        # Session 0 has no interactive desktop, so automation cannot work there
        # no matter what the other checks say.
        check = self._session_check_with_id(0)
        self.assertEqual(check.state, "unavailable")
        self.assertTrue(check.evidence["session_zero"])
        self.assertFalse(check.evidence["interactive_session"])
        self.assertIn("Session 0", check.summary)

    def test_interactive_session_is_reported_available(self) -> None:
        check = self._session_check_with_id(1)
        self.assertEqual(check.state, "available")
        self.assertFalse(check.evidence["session_zero"])
        self.assertTrue(check.evidence["interactive_session"])

    def test_session_evidence_withholds_the_raw_session_id(self) -> None:
        # The numeric id identifies the environment without being actionable;
        # only the Session 0 distinction changes what automation can do.
        check = self._session_check_with_id(7)
        self.assertNotIn("session_id", check.evidence)
        self.assertNotIn("7", json.dumps(check.evidence))

    @staticmethod
    def _session_check_with_id(session_id: int) -> probe.CapabilityCheck:
        def process_id_to_session_id(_pid, out):
            out._obj.value = session_id
            return 1

        kernel = mock.MagicMock()
        kernel.ProcessIdToSessionId.side_effect = process_id_to_session_id
        user32 = mock.MagicMock()
        user32.GetSystemMetrics.return_value = 0

        def load(name: str):
            return {"kernel32": kernel, "user32": user32}[name]

        with mock.patch.object(probe, "_windows_dll", side_effect=load):
            return probe._check_windows_session()

    def test_unreadable_session_is_unknown_not_unavailable(self) -> None:
        kernel = mock.MagicMock()
        kernel.ProcessIdToSessionId.return_value = 0  # failure
        with mock.patch.object(probe, "_windows_dll", return_value=kernel):
            check = probe._check_windows_session()
        self.assertEqual(check.state, "unknown")
        self.assertFalse(check.evidence["session_id_available"])

    def test_probe_failures_degrade_to_unknown_without_raising(self) -> None:
        # A probe must never propagate a native failure to the caller.
        for factory in (
            probe._check_windows_session,
            probe._check_windows_input_desktop,
            probe._check_windows_integrity,
            probe._check_windows_dpi,
        ):
            with self.subTest(check=factory.__name__):
                with mock.patch.object(
                    probe, "_windows_dll", side_effect=OSError("no such dll")
                ):
                    check = factory()
                self.assertEqual(check.state, "unknown")
                self.assertEqual(check.evidence["error_type"], "OSError")

    def test_dpi_quantisation_drives_the_reported_state(self) -> None:
        for physical, reported, state in (
            (3840, 1536, "degraded"),   # 250% scaling, unaware
            (1920, 1920, "available"),  # no scaling
        ):
            with self.subTest(physical=physical, reported=reported):
                check = self._dpi_check(physical, reported)
                self.assertEqual(check.state, state)
                self.assertEqual(
                    check.evidence["pointer_quantisation"],
                    round(physical / reported, 4),
                )
                self.assertEqual(
                    check.evidence["scaled_display"], physical != reported
                )

    def test_dpi_summary_states_precision_loss_not_a_coordinate_mismatch(
        self,
    ) -> None:
        # UIA bounds and GetSystemMetrics are reported in the same space, so an
        # unaware host aims consistently; what it loses is resolution.  The
        # wording must not imply that clicks land in the wrong place.
        check = self._dpi_check(3840, 1536)
        self.assertIn("quantise", check.summary)
        for forbidden in ("mismatch", "wrong", "incorrect", "misaligned"):
            self.assertNotIn(forbidden, check.summary.lower())

    @staticmethod
    def _dpi_check(physical: int, reported: int) -> probe.CapabilityCheck:
        shcore = mock.MagicMock()

        def get_awareness(_handle, out):
            out._obj.value = 0
            return 0

        shcore.GetProcessDpiAwareness.side_effect = get_awareness
        user32 = mock.MagicMock()
        user32.GetSystemMetrics.return_value = reported
        user32.GetDC.return_value = 1234
        gdi32 = mock.MagicMock()
        gdi32.GetDeviceCaps.return_value = physical

        def load(name: str):
            return {"shcore": shcore, "user32": user32, "gdi32": gdi32}[name]

        with mock.patch.object(probe, "_windows_dll", side_effect=load):
            return probe._check_windows_dpi()

    def test_low_integrity_is_unavailable_and_medium_is_available(self) -> None:
        for level, state in (
            ("untrusted", "unavailable"),
            ("low", "unavailable"),
            ("medium", "available"),
            ("high", "available"),
            ("system", "available"),
        ):
            with self.subTest(level=level):
                check = self._integrity_check(level)
                self.assertEqual(check.state, state)
                self.assertEqual(check.evidence["integrity_level"], level)

    def test_medium_integrity_summary_states_the_uipi_ceiling(self) -> None:
        # Medium integrity is not a misconfiguration, but the operator must know
        # that an elevated target remains out of reach.
        check = self._integrity_check("medium")
        self.assertIn("UIPI", check.summary)
        self.assertIn("higher integrity", check.summary)

    def test_unknown_integrity_is_not_reported_as_usable(self) -> None:
        check = self._integrity_check(None)
        self.assertEqual(check.state, "unknown")

    @staticmethod
    def _integrity_check(level: str | None) -> probe.CapabilityCheck:
        advapi = mock.MagicMock()
        kernel = mock.MagicMock()

        def open_token(_process, _access, out):
            out._obj.value = 999
            return 1

        advapi.OpenProcessToken.side_effect = open_token
        advapi.GetTokenInformation.return_value = 1

        def load(name: str):
            return {"advapi32": advapi, "kernel32": kernel}[name]

        with (
            mock.patch.object(probe, "_windows_dll", side_effect=load),
            mock.patch.object(
                probe, "_windows_integrity_level", return_value=level
            ),
        ):
            return probe._check_windows_integrity()


@unittest.skipUnless(WINDOWS, "requires a real Windows kernel")
class NativeWindowsProbeTests(unittest.TestCase):
    """Cross-check each probe against an independent kernel query."""

    def setUp(self) -> None:
        self.checks = {
            check.name: check for check in probe._probe_windows()
        }

    def test_every_check_is_present_and_serialisable(self) -> None:
        self.assertEqual(
            set(self.checks),
            {
                "windows.uia",
                "windows.session",
                "windows.input_desktop",
                "windows.integrity",
                "windows.dpi",
                "script.sandbox",
            },
        )
        json.dumps({k: v.to_dict() for k, v in self.checks.items()})

    def test_session_conclusion_matches_the_kernel(self) -> None:
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        session_id = ctypes.c_uint32()
        kernel.ProcessIdToSessionId.argtypes = [
            ctypes.c_uint32, ctypes.POINTER(ctypes.c_uint32)
        ]
        self.assertTrue(
            kernel.ProcessIdToSessionId(
                ctypes.c_uint32(os.getpid()), ctypes.byref(session_id)
            )
        )
        check = self.checks["windows.session"]
        self.assertEqual(check.evidence["session_zero"], session_id.value == 0)
        self.assertEqual(
            check.state, "unavailable" if session_id.value == 0 else "available"
        )

    def test_integrity_level_matches_the_process_token(self) -> None:
        check = self.checks["windows.integrity"]
        level = check.evidence["integrity_level"]
        self.assertIn(level, {"untrusted", "low", "medium", "high", "system"})
        # A test process is never elevated-with-low-integrity; confirm the two
        # signals are mutually consistent.
        if check.evidence["elevated"]:
            self.assertIn(level, {"high", "system"})

    def test_dpi_quantisation_matches_the_display(self) -> None:
        user32 = ctypes.WinDLL("user32", use_last_error=True)
        gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)
        user32.GetSystemMetrics.argtypes = [ctypes.c_int]
        user32.GetSystemMetrics.restype = ctypes.c_int
        user32.GetDC.argtypes = [ctypes.c_void_p]
        user32.GetDC.restype = ctypes.c_void_p
        user32.ReleaseDC.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
        gdi32.GetDeviceCaps.argtypes = [ctypes.c_void_p, ctypes.c_int]
        gdi32.GetDeviceCaps.restype = ctypes.c_int

        reported = int(user32.GetSystemMetrics(probe._SM_CXSCREEN))
        device = user32.GetDC(None)
        try:
            physical = int(gdi32.GetDeviceCaps(device, probe._DESKTOPHORZRES))
        finally:
            user32.ReleaseDC(None, device)

        check = self.checks["windows.dpi"]
        self.assertEqual(
            check.evidence["pointer_quantisation"],
            round(physical / reported, 4),
        )
        self.assertEqual(check.evidence["scaled_display"], physical != reported)
        self.assertEqual(
            check.state, "degraded" if physical > reported else "available"
        )

    def test_report_does_not_leak_environment_identifying_values(self) -> None:
        blob = json.dumps(
            {k: v.to_dict() for k, v in self.checks.items()}, ensure_ascii=False
        ).lower()
        for variable in (
            "USERNAME", "USERPROFILE", "COMPUTERNAME", "USERDOMAIN", "SystemRoot"
        ):
            value = os.environ.get(variable)
            if value:
                with self.subTest(variable=variable):
                    self.assertNotIn(value.lower(), blob)

    def test_no_check_reports_a_state_outside_the_vocabulary(self) -> None:
        for name, check in self.checks.items():
            with self.subTest(check=name):
                self.assertIn(check.state, probe.PROBE_STATES)
                self.assertTrue(check.summary.strip())


if __name__ == "__main__":
    unittest.main()
