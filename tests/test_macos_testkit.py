"""Cross-platform contracts for the self-contained macOS AX test kit."""

from __future__ import annotations

import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile
import unittest


PROJECT_ROOT = Path(__file__).resolve().parents[1]
TESTKIT_ROOT = PROJECT_ROOT / "tests" / "macos"
RUN_SCRIPT = TESTKIT_ROOT / "run.sh"
BUILD_SCRIPT = TESTKIT_ROOT / "build.sh"
RUNNER_SOURCE = TESTKIT_ROOT / "AXTestRunner.swift"
FIXTURE_SOURCE = TESTKIT_ROOT / "FixtureApp.swift"
SHELL_SCRIPTS = (RUN_SCRIPT, BUILD_SCRIPT)
FIXTURE_BUNDLE_ID = "dev.ai-auto-desktop.testkit.fixture"
RUNNER_BUNDLE_ID = "dev.ai-auto-desktop.testkit.ax-runner"
ARCHIVE_MEMBERS = {
    "report.json",
    "README.txt",
    "identity.txt",
    "SHA256SUMS",
}


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def _tree_snapshot(root: Path) -> tuple[str, ...] | None:
    """Return names only, so the check does not depend on GNU stat/find."""

    if not root.exists():
        return None
    return tuple(
        sorted(
            f"{path.relative_to(root)}{'/' if path.is_dir() else ''}"
            for path in root.rglob("*")
        )
    )


def _decode_single_json(payload: str) -> object:
    stripped = payload.lstrip()
    if not stripped:
        raise ValueError("stdout is empty")
    value, end = json.JSONDecoder().raw_decode(stripped)
    if stripped[end:].strip():
        raise ValueError("stdout contains data after the JSON document")
    return value


class MacOSTestkitShellContracts(unittest.TestCase):
    def test_shell_scripts_are_executable(self) -> None:
        for script in SHELL_SCRIPTS:
            with self.subTest(script=script.name):
                self.assertTrue(script.is_file())
                self.assertTrue(os.access(script, os.X_OK))

    def test_shell_scripts_pass_posix_shell_syntax_check(self) -> None:
        shell = shutil.which("sh")
        if shell is None:
            self.skipTest("a POSIX sh is unavailable on this host")

        for script in SHELL_SCRIPTS:
            with self.subTest(script=script.name):
                completed = subprocess.run(
                    [shell, "-n", str(script)],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    encoding="utf-8",
                    check=False,
                )
                self.assertEqual(
                    completed.returncode,
                    0,
                    msg=f"{script.name}: {completed.stderr}",
                )


class MacOSTestkitSourceContracts(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.runner = _read(RUNNER_SOURCE)
        cls.fixture = _read(FIXTURE_SOURCE)
        cls.build = _read(BUILD_SCRIPT)
        cls.native_sources = cls.runner + "\n" + cls.fixture

    def test_native_sources_never_request_or_capture_the_screen(self) -> None:
        forbidden_apis = (
            "CGRequestScreenCaptureAccess",
            "CGWindowListCreateImage",
            "CGWindowListCreateImageFromArray",
            "CGDisplayCreateImage",
            "CGDisplayCreateImageForRect",
            "CGDisplayStreamCreate",
            "SCScreenshotManager",
            "SCStream",
            "AVCaptureScreenInput",
            "screencapture",
        )
        for api in forbidden_apis:
            with self.subTest(api=api):
                self.assertNotIn(api, self.native_sources)
        self.assertIn("CGPreflightScreenCaptureAccess()", self.runner)
        self.assertIn("AXIsProcessTrusted()", self.runner)

    def test_accessibility_root_is_scoped_to_the_owned_fixture_pid(self) -> None:
        self.assertNotIn("AXUIElementCreateSystemWide", self.native_sources)
        calls = re.findall(
            r"AXUIElementCreateApplication\s*\(\s*([^)]*?)\s*\)",
            self.runner,
        )
        self.assertEqual(calls, ["fixturePID"])
        self.assertIn("AXUIElementCreateApplication(fixturePID)", self.runner)
        self.assertIn("getpgid(fixturePID) == getpgrp()", self.runner)

    def test_mutations_have_settable_and_action_preflight_checks(self) -> None:
        for marker in (
            "AXUIElementIsAttributeSettable",
            "AXUIElementCopyActionNames",
            "supportedActions.contains(kAXPressAction as String)",
        ):
            with self.subTest(marker=marker):
                self.assertIn(marker, self.runner)

        ordered_markers = (
            (
                "let (focusPreflightError, focusSettable) = settable(",
                "let focusError = AXUIElementSetAttributeValue",
            ),
            (
                "let (valuePreflightError, valueSettable) = settable(",
                "let valueError = AXUIElementSetAttributeValue",
            ),
            (
                "let (actionPreflightError, supportedActions) = actions(",
                "let pressError = AXUIElementPerformAction",
            ),
        )
        for preflight, mutation in ordered_markers:
            with self.subTest(mutation=mutation):
                self.assertLess(
                    self.runner.index(preflight), self.runner.index(mutation)
                )

    def test_children_traversal_is_bounded(self) -> None:
        for marker in (
            "AXUIElementGetAttributeValueCount",
            "AXUIElementCopyAttributeValues",
            "let requested = min(Int(count), max(0, remaining))",
            "maxDepth = min(value, 32)",
            "maxNodes = min(value, 2_048)",
        ):
            with self.subTest(marker=marker):
                self.assertIn(marker, self.runner)
        self.assertIsNone(
            re.search(
                r"AXUIElementCopyAttributeValue\s*\(\s*element\s*,"
                r"\s*kAXChildrenAttribute",
                self.runner,
            )
        )

    def test_each_mutation_uses_a_freshly_resolved_element_and_rereads(self) -> None:
        for marker in (
            "func freshSnapshot() -> Snapshot",
            "func freshNode(identifier: String, role: String) -> Node?",
            "focus target re-resolve failed",
            "value target re-resolve failed",
            "press target re-resolve failed",
            "let afterFocus = freshSnapshot()",
            "let afterValue = freshSnapshot()",
            'let current = freshSnapshot()',
            'current, identifier: "fixture-status"',
        ):
            with self.subTest(marker=marker):
                self.assertIn(marker, self.runner)

    def test_runner_and_built_apps_use_fixed_bundle_ids(self) -> None:
        self.assertIn(
            f'private let fixtureBundleID = "{FIXTURE_BUNDLE_ID}"',
            self.runner,
        )
        self.assertIn(
            f'private let runnerBundleID = "{RUNNER_BUNDLE_ID}"',
            self.runner,
        )
        for bundle_id in (FIXTURE_BUNDLE_ID, RUNNER_BUNDLE_ID):
            with self.subTest(bundle_id=bundle_id):
                self.assertIn(
                    f"<key>CFBundleIdentifier</key><string>{bundle_id}</string>",
                    self.build,
                )
        for marker in (
            "codesign --verify --strict",
            "designated =>",
            "CDHash=",
            "architectures=",
        ):
            with self.subTest(marker=marker):
                self.assertIn(marker, self.build)

    def test_launcher_uses_launch_services_and_report_as_source_of_truth(self) -> None:
        launcher = _read(RUN_SCRIPT)
        self.assertIn('/usr/bin/open -n -W "$runner_app" --args', launcher)
        self.assertIn('--report "$temporary_report"', launcher)
        self.assertIn('--pid-file "$runner_pid_file"', launcher)
        self.assertIn('--cancel-file "$cancel_file"', launcher)
        self.assertIn('plutil -extract status raw', launcher)


@unittest.skipIf(platform.system() == "Darwin", "exercises the non-macOS shell path")
class NonMacOSLauncherContracts(unittest.TestCase):
    def test_unsupported_host_emits_one_json_and_minimal_archive(self) -> None:
        shell = shutil.which("sh")
        if shell is None:
            self.skipTest("a POSIX sh is unavailable on this host")
        if shutil.which("tar") is None:
            self.skipTest("tar is unavailable on this host")

        default_output = TESTKIT_ROOT / "results"
        default_build = TESTKIT_ROOT / ".build"
        before_output = _tree_snapshot(default_output)
        before_build = _tree_snapshot(default_build)

        try:
            with tempfile.TemporaryDirectory() as temporary:
                temporary_root = Path(temporary)
                output_root = temporary_root / "output"
                build_root = temporary_root / "build"
                completed = subprocess.run(
                    [
                        shell,
                        str(RUN_SCRIPT),
                        "--output",
                        str(output_root),
                        "--build-dir",
                        str(build_root),
                    ],
                    cwd=temporary_root,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    encoding="utf-8",
                    check=False,
                    timeout=30,
                )

                self.assertEqual(
                    completed.returncode, 3, msg=completed.stderr
                )
                report = _decode_single_json(completed.stdout)
                self.assertIsInstance(report, dict)
                assert isinstance(report, dict)
                self.assertEqual(report["status"], "unsupported")
                self.assertEqual(report["message"], "requires_macos")
                self.assertFalse(
                    report["permissions"]["screen_capture"][
                        "request_attempted"
                    ]
                )
                self.assertFalse(
                    report["permissions"]["screen_capture"][
                        "capture_attempted"
                    ]
                )

                result_directories = list(output_root.iterdir())
                self.assertEqual(len(result_directories), 1)
                result_directory = result_directories[0]
                self.assertTrue(result_directory.is_dir())
                archive_path = (
                    result_directory / "macos-ax-test-result.tar.gz"
                )
                self.assertTrue(archive_path.is_file())
                self.assertEqual(
                    {path.name for path in result_directory.iterdir()},
                    ARCHIVE_MEMBERS | {archive_path.name},
                )

                with tarfile.open(archive_path, mode="r:gz") as archive:
                    members = archive.getmembers()
                    self.assertEqual(
                        {member.name for member in members}, ARCHIVE_MEMBERS
                    )
                    self.assertEqual(len(members), len(ARCHIVE_MEMBERS))
                    self.assertTrue(all(member.isfile() for member in members))
                    archived_report_file = archive.extractfile("report.json")
                    self.assertIsNotNone(archived_report_file)
                    assert archived_report_file is not None
                    archived_report = json.load(archived_report_file)
                self.assertEqual(archived_report, report)
        finally:
            self.assertEqual(_tree_snapshot(default_output), before_output)
            self.assertEqual(_tree_snapshot(default_build), before_build)


if __name__ == "__main__":
    unittest.main()
