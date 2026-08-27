"""Cross-platform contracts for the self-contained macOS AX test kit."""

from __future__ import annotations

import json
import hashlib
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
ARCHIVE_SCRIPT = TESTKIT_ROOT / "archive.sh"
IDENTITY_SCRIPT = TESTKIT_ROOT / "identity.sh"
PACKAGE_SCRIPT = TESTKIT_ROOT / "package-source.sh"
SOURCE_PROVENANCE_SCRIPT = TESTKIT_ROOT / "source-provenance.sh"
SOURCE_PACKAGE_MANIFEST = TESTKIT_ROOT / "SOURCE_PACKAGE_FILES.txt"
RUNNER_SOURCE = TESTKIT_ROOT / "AXTestRunner.swift"
FIXTURE_SOURCE = TESTKIT_ROOT / "FixtureApp.swift"
SHELL_SCRIPTS = (
    RUN_SCRIPT,
    BUILD_SCRIPT,
    ARCHIVE_SCRIPT,
    IDENTITY_SCRIPT,
    PACKAGE_SCRIPT,
    SOURCE_PROVENANCE_SCRIPT,
)
EXECUTABLE_SHELL_SCRIPTS = (RUN_SCRIPT, BUILD_SCRIPT, PACKAGE_SCRIPT)
FIXTURE_BUNDLE_ID = "dev.ai-auto-desktop.testkit.fixture"
RUNNER_BUNDLE_ID = "dev.ai-auto-desktop.testkit.ax-runner"
ARCHIVE_MEMBERS = {
    "report.json",
    "README.txt",
    "identity.txt",
    "SHA256SUMS",
}
NORMALIZED_MTIME = 946_684_800
SOURCE_PACKAGE_MEMBERS = {
    line
    for line in SOURCE_PACKAGE_MANIFEST.read_text(encoding="utf-8").splitlines()
    if line and not line.startswith("#")
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
        for script in EXECUTABLE_SHELL_SCRIPTS:
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
        cls.launcher = _read(RUN_SCRIPT)
        cls.archive = _read(ARCHIVE_SCRIPT)
        cls.identity = _read(IDENTITY_SCRIPT)
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

    def test_cfarray_values_promote_to_optional_and_fail_closed(self) -> None:
        self.assertEqual(
            self.runner.count(
                "let rawPointer: UnsafeRawPointer? = "
                "CFArrayGetValueAtIndex(values, index)"
            ),
            2,
        )
        self.assertEqual(
            self.runner.count("guard let pointer = rawPointer else {"), 2
        )
        self.assertNotRegex(
            self.runner,
            r"guard let pointer = CFArrayGetValueAtIndex",
        )

    def test_private_runner_types_do_not_escape_through_top_level_values(
        self,
    ) -> None:
        self.assertIn("private struct Arguments", self.runner)
        self.assertIn("private struct Outcome", self.runner)
        self.assertIn("private func main() -> Never", self.runner)
        main = self.runner[self.runner.index("private func main() -> Never") :]
        for marker in (
            "let parsedArguments = parseArguments()",
            "let outcome = run(arguments: parsedArguments)",
            "let reportDestination = parsedArguments?.reportPath",
        ):
            with self.subTest(marker=marker):
                self.assertIn(marker, main)
        prefix = self.runner[: self.runner.index("private func main() -> Never")]
        self.assertNotRegex(prefix, r"(?m)^let parsedArguments =")
        self.assertNotRegex(prefix, r"(?m)^let outcome =")

    def test_each_mutation_uses_a_freshly_resolved_element_and_rereads(self) -> None:
        for marker in (
            "func freshSnapshot() -> Snapshot",
            "func freshNode(identifier: String, role: String) -> Node?",
            "focus target re-resolve failed",
            "value target re-resolve failed",
            "press target re-resolve failed",
            "pointer target re-resolve failed",
            "let afterFocus = freshSnapshot()",
            "let afterValue = freshSnapshot()",
            'let current = freshSnapshot()',
            'current, identifier: "fixture-status"',
        ):
            with self.subTest(marker=marker):
                self.assertIn(marker, self.runner)

    def test_pointer_click_uses_fresh_fixture_ax_bounds_pid_and_reread(self) -> None:
        for marker in (
            'setAccessibilityIdentifier("fixture-pointer")',
            'setAccessibilityIdentifier("fixture-pointer-status")',
            '@objc private func recordPointerClick()',
            'private let expectedPointerStatus = "Pointer status: clicked"',
            'identifier: "fixture-pointer"',
            'identifier: "fixture-pointer-status"',
            'let beforePointer = freshSnapshot()',
            'let pointerBounds = elementBounds(',
            '$0.width > 0 && $0.height > 0',
            'AXUIElementGetPid(',
            'pointerTargetPID == fixturePID',
            'NSWorkspace.shared.frontmostApplication?.processIdentifier == fixturePID',
            'let center = CGPoint(x: bounds.midX, y: bounds.midY)',
            'AXUIElementCopyElementAtPosition(',
            'CFEqual($0, pointerButton.element)',
            'pointerDispatch = postLeftClick(',
            'at: center, to: pointerTargetPID',
            'let afterPointer = freshSnapshot()',
            '"id": "pointer_click_and_reread"',
            '"status_matches_from_fresh_snapshot"',
        ):
            with self.subTest(marker=marker):
                self.assertIn(marker, self.native_sources)

        pointer_block = self.runner[
            self.runner.index("let beforePointer = freshSnapshot()") :
            self.runner.index("private func emit(")
        ]
        for preflight in (
            'findOne(\n              beforePointer, identifier: "fixture-pointer"',
            "let pointerBounds = elementBounds(",
            "pointerTargetPID == fixturePID",
            "let center = CGPoint(x: bounds.midX, y: bounds.midY)",
            "pointerHitTestMatchesTarget = pointerHitTestError == .success",
            "pointerStatusIdleBeforeDispatch",
        ):
            with self.subTest(preflight=preflight):
                self.assertLess(
                    pointer_block.index(preflight),
                    pointer_block.index("pointerDispatch = postLeftClick("),
                )
        self.assertLess(
            self.runner.index("pointerDispatch = postLeftClick("),
            self.runner.index("let afterPointer = freshSnapshot()"),
        )

    def test_pointer_click_posts_only_pid_scoped_left_click_events(self) -> None:
        pointer_dispatch = self.runner[
            self.runner.index("private func postLeftClick") :
            self.runner.index("private func typeTextTargetIsEligible")
        ]
        for marker in (
            "mouseType: .mouseMoved",
            "mouseType: .leftMouseDown",
            "mouseType: .leftMouseUp",
            "move.postToPid(pid)",
            "down.postToPid(pid)",
            "up.postToPid(pid)",
            "frontmostAtDispatch",
            "NSWorkspace.shared.frontmostApplication?.processIdentifier == pid",
        ):
            with self.subTest(marker=marker):
                self.assertIn(marker, pointer_dispatch)
        self.assertNotIn("CGEventPost(", pointer_dispatch)
        self.assertNotIn(".post(tap:", pointer_dispatch)
        self.assertNotIn("CGWarpMouseCursorPosition", pointer_dispatch)
        self.assertNotIn("CGEventSource", pointer_dispatch)

    def test_pointer_click_never_uses_screen_capture_ocr_or_raw_coordinates(self) -> None:
        for marker in (
            "screenshot", "Screenshot", "VNRecognizeTextRequest",
            "import Vision",
            "CGWindowListCreateImage", "CGDisplayCreateImage",
            "CGWarpMouseCursorPosition",
        ):
            with self.subTest(marker=marker):
                self.assertNotIn(marker, self.native_sources)
        self.assertNotRegex(
            self.runner,
            r"CGPoint\s*\(\s*x:\s*-?\d+(?:\.\d+)?\s*,"
            r"\s*y:\s*-?\d+(?:\.\d+)?\s*\)",
        )

    def test_type_text_covers_unicode_fresh_snapshots_and_secure_rejection(self) -> None:
        for marker in (
            'private let typeTextValue = "ASCII 中文 😀"',
            'let typeTextSegments = ["ASCII ", "中文 ", "😀"]',
            'identifier: "fixture-input", role: kAXTextFieldRole as String',
            "focus_verified_before_dispatch",
            "value_matches_from_fresh_snapshot",
            'let afterType = freshSnapshot()',
            'private func postUnicodeText(_ text: String, to pid: pid_t) -> TypeTextDispatch',
            "private func typeTextTargetIsEligible(_ node: Node) -> Bool",
            "keyboardSetUnicodeString",
            "keyDown.postToPid(pid)",
            "keyUp.postToPid(pid)",
            'identifier: "fixture-secure-input"',
            '"id": "type_text_secure_rejected"',
            "let secureRejected = !typeTextTargetIsEligible(initialSecureInput)",
            '"event_post_attempted": false',
            '"value_read": false',
            '"utf16_units_posted": dispatch.utf16UnitsPosted',
            '"event_submitted": dispatch.submitted',
            "IsSecureEventInputEnabled()",
            "private func secureEventInputIsEnabled(_ value: Bool) -> Bool",
            "private func secureEventInputIsEnabled(_ value: UInt8) -> Bool",
            "secure_event_input_enabled_before_dispatch",
            "secure_event_input_checked_before_dispatch",
            "let protected = role == \"AXSecureTextField\"",
        ):
            with self.subTest(marker=marker):
                self.assertIn(marker, self.runner)
        self.assertNotIn("IsSecureEventInputEnabled() != 0", self.runner)
        self.assertIn(
            "secureInput = NSSecureTextField(",
            self.fixture,
        )
        self.assertIn(
            'secureInput.stringValue = "fixture-secret"',
            self.fixture,
        )
        self.assertIn(
            "value: protected ? nil : attribute(",
            self.runner,
        )
        self.assertLess(
            self.runner.index("let protected = role == \"AXSecureTextField\""),
            self.runner.index("value: protected ? nil : attribute("),
        )
        type_text_block = self.runner[
            self.runner.index("let typeTextSegments") :
            self.runner.index("guard let pressButton")
        ]
        self.assertLess(
            type_text_block.index("freshNode("),
            type_text_block.index("dispatch = postUnicodeText(segment, to: fixturePID)"),
        )
        self.assertLess(
            type_text_block.index("let afterType = freshSnapshot()"),
            type_text_block.index("value_matches_from_fresh_snapshot"),
        )

    def test_event_input_stays_fixture_pid_scoped_without_global_post(self) -> None:
        self.assertNotIn("CGEventPost(", self.native_sources)
        self.assertNotIn(".post(tap:", self.native_sources)
        self.assertNotIn("CGWarpMouseCursorPosition", self.native_sources)
        self.assertIn("NSWorkspace.shared.frontmostApplication", self.runner)
        self.assertIn("postUnicodeText(segment, to: fixturePID)", self.runner)
        self.assertIn("postLeftClick(", self.runner)
        self.assertIn("at: center, to: pointerTargetPID", self.runner)
        self.assertIn("import Carbon.HIToolbox", self.runner)
        self.assertIn("-framework Carbon", self.build)

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
                self.assertIn(marker, self.build + "\n" + self.identity)

    def test_launcher_uses_launch_services_and_report_as_source_of_truth(self) -> None:
        self.assertIn(
            '/usr/bin/open -n -W "$runner_app" --args', self.launcher
        )
        self.assertIn('--report "$temporary_report"', self.launcher)
        self.assertIn('--pid-file "$runner_pid_file"', self.launcher)
        self.assertIn('--cancel-file "$cancel_file"', self.launcher)
        self.assertIn('plutil -extract status raw', self.launcher)
        timeout_branch = self.launcher.index('runner_status" -eq 124')
        missing_report_branch = self.launcher.index(
            '! -s "$temporary_report"'
        )
        self.assertLess(timeout_branch, missing_report_branch)
        self.assertIn('runner_timeout', self.launcher)
        self.assertIn('result_archive_failed', self.launcher)
        self.assertIn('archive_hash_failed', self.launcher)

    def test_launcher_has_bounded_configurable_timeout(self) -> None:
        self.assertIn('--timeout SECONDS', self.launcher)
        self.assertIn('[ "$2" -lt 1 ]', self.launcher)
        self.assertIn('[ "$2" -gt 600 ]', self.launcher)
        self.assertIn(
            '"timeout_seconds":$runner_timeout_seconds', self.launcher
        )
        self.assertIn(
            '"runner_pid_observed":$runner_pid_observed', self.launcher
        )

    def test_source_provenance_is_passed_to_runner_and_reported(self) -> None:
        for marker in (
            '--source-revision "$source_revision"',
            '--source-worktree "$source_worktree"',
            '--source-package-digest "$source_package_digest"',
        ):
            with self.subTest(marker=marker):
                self.assertIn(marker, self.launcher)
        for marker in (
            'case "--source-revision"',
            'case "--source-worktree"',
            'case "--source-package-digest"',
            '"revision": reportSourceRevision',
            '"worktree": reportSourceWorktree',
            '"package_digest": reportSourcePackageDigest',
            'isLowercaseHex(raw[index + 1], lengths: [40, 64])',
            'isLowercaseHex(raw[index + 1], lengths: [64])',
        ):
            with self.subTest(marker=marker):
                self.assertIn(marker, self.runner)

    def test_build_declares_and_checks_minimum_swift_version(self) -> None:
        self.assertIn('"$swiftc_path" --version', self.build)
        self.assertIn('至少需要 Swift 5.3', self.build)
        self.assertIn('exit 81', self.build)

    def test_compile_diagnostics_are_bounded_sanitized_and_archived_in_readme(
        self,
    ) -> None:
        for marker in (
            'compile_diagnostics="$build_root/compile-diagnostics.txt"',
            "max_lines=120",
            "max_output_bytes=12288",
            "max_line_bytes = 512",
            "LC_ALL=C head -c 131072",
            "compile_status_file=$compile_log.status",
            "compile_overflow_file=$compile_log.overflow",
            "compile_overflow_bytes=$(LC_ALL=C wc -c",
            "raw_capture_limit_bytes=131072",
            "raw_capture_truncated=",
            'replace_literal(value, diagnostic_script_dir, "<TESTKIT>")',
            'replace_literal(value, diagnostic_build_root, "<BUILD>")',
            'replace_literal(value, diagnostic_sdk_path, "<SDK>")',
            '"[compiler output truncated]"',
        ):
            with self.subTest(marker=marker):
                self.assertIn(marker, self.build)
        for marker in (
            'compile_diagnostics_size" -le 16384',
            "--- sanitized Swift compile diagnostics ---",
            'cat "$compile_diagnostics_path"',
            'rm -f "$build_root/compile-diagnostics.txt"',
        ):
            with self.subTest(marker=marker):
                self.assertIn(marker, self.launcher)
        self.assertNotIn(
            "compile-diagnostics.txt SHA256SUMS", self.launcher
        )

    def test_result_archiver_normalizes_metadata_portably(self) -> None:
        self.assertIn(
            'create_normalized_tar_gz "$archive_path"', self.launcher
        )
        for marker in (
            "COPYFILE_DISABLE=1",
            "--format ustar",
            "--uid 0 --gid 0 --uname root --gname root",
            "--no-acls --no-fflags --no-xattrs --no-mac-metadata",
            "--format=ustar --owner=0 --group=0 --numeric-owner",
            "--no-acls --no-selinux --no-xattrs",
            "touch -t 200001010000",
            '"$normalized_archive_gzip" -n -c',
            'od -An -tu1 -j 8 -N 2',
            '[ "$1" != 0 ]',
            '[ "$2" != 3 ]',
        ):
            with self.subTest(marker=marker):
                self.assertIn(marker, self.archive)
        self.assertNotIn("tar -czf", self.launcher)
        self.assertNotIn("sha256_manifest=unavailable", self.launcher)

    def test_identity_attestation_avoids_lossy_tool_pipelines(self) -> None:
        self.assertIn("write_identity_attestation", self.build)
        self.assertLess(
            self.build.index('rm -f "$attestation"'),
            self.build.index('uname -s'),
        )
        self.assertNotRegex(
            self.build + "\n" + self.identity,
            r"(?:codesign|swiftc_path|identity_swiftc|identity_codesign|"
            r"identity_lipo|identity_shasum)[^\n]*\|",
        )
        for marker in (
            "designated requirement 为空或无效",
            "Identifier 为空或无效",
            "CDHash 为空或无效",
            "architectures 为空或无效",
            "sha256 为空或无效",
            "swift version 为空或无效",
            "identity stability 为空或无效",
        ):
            with self.subTest(marker=marker):
                self.assertIn(marker, self.identity)

    def test_source_package_manifest_is_complete_and_minimal(self) -> None:
        self.assertEqual(
            SOURCE_PACKAGE_MEMBERS,
            {
                "README.md",
                "run.sh",
                "build.sh",
                "archive.sh",
                "identity.sh",
                "source-provenance.sh",
                "FixtureApp.swift",
                "AXTestRunner.swift",
                "SOURCE_PACKAGE_FILES.txt",
                "package-source.sh",
            },
        )
        for member in SOURCE_PACKAGE_MEMBERS:
            with self.subTest(member=member):
                member_path = TESTKIT_ROOT / member
                self.assertTrue(member_path.is_file())
                self.assertFalse(member_path.is_symlink())
        manifest_text = SOURCE_PACKAGE_MANIFEST.read_text(encoding="utf-8")
        self.assertIn("# type_text/pointer_click sources:", manifest_text)
        self.assertIn("# FixtureApp.swift AXTestRunner.swift", manifest_text)

    def test_source_package_is_reproducible_and_manifest_covers_members(
        self,
    ) -> None:
        if shutil.which("tar") is None or shutil.which("gzip") is None:
            self.skipTest("tar and gzip are required")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archives = (root / "first.tar.gz", root / "second.tar.gz")
            for archive_path in archives:
                completed = subprocess.run(
                    [str(PACKAGE_SCRIPT), "--allow-dirty", str(archive_path)],
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                    text=True, encoding="utf-8", check=False,
                )
                self.assertEqual(completed.returncode, 0, msg=completed.stderr)
                self.assertRegex(
                    completed.stderr, r"源码 revision：[0-9a-f]{40,64}"
                )
                self.assertRegex(
                    completed.stderr, r"源码内容 SHA-256：[0-9a-f]{64}"
                )
            self.assertEqual(archives[0].read_bytes(), archives[1].read_bytes())
            with tarfile.open(archives[0], "r:gz") as archive:
                contents = {
                    member.name: archive.extractfile(member).read()
                    for member in archive.getmembers()
                }
            manifest = contents.pop("SOURCE_MANIFEST.txt").decode()
            lines = manifest.splitlines()
            self.assertEqual(
                lines[0],
                "schema_version=ai-auto-desktop.macos-source-manifest/v1",
            )
            self.assertRegex(lines[2], r"^source_worktree=(?:clean|dirty)$")
            self.assertEqual(lines[3], f"member_count={len(contents)}")
            expected_file_lines = []
            for member in SOURCE_PACKAGE_MANIFEST.read_text(
                encoding="utf-8"
            ).splitlines():
                if not member or member.startswith("#"):
                    continue
                mode = "0755" if os.access(TESTKIT_ROOT / member, os.X_OK) else "0644"
                expected_file_lines.append(
                    f"file={mode}:{hashlib.sha256(contents[member]).hexdigest()}:{member}"
                )
            self.assertEqual(lines[4:], expected_file_lines)

    def test_source_package_rejects_dirty_release_without_override(self) -> None:
        git = shutil.which("git")
        if git is None:
            self.skipTest("git is required")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            repository = root / "repository"
            copied_testkit = repository / "tests" / "macos"
            copied_testkit.parent.mkdir(parents=True)
            shutil.copytree(TESTKIT_ROOT, copied_testkit)
            subprocess.run([git, "init", "-q"], cwd=repository, check=True)
            subprocess.run([git, "add", "tests/macos"], cwd=repository, check=True)
            subprocess.run(
                [git, "-c", "user.name=Test", "-c",
                 "user.email=test@example.invalid", "commit", "-qm",
                 "fixture baseline"],
                cwd=repository, check=True,
            )
            with (copied_testkit / "README.md").open("a", encoding="utf-8") as stream:
                stream.write("dirty test\n")
            archive = root / "release.tar.gz"
            completed = subprocess.run(
                [str(copied_testkit / "package-source.sh"), str(archive)],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                text=True, encoding="utf-8", check=False,
            )
            self.assertEqual(completed.returncode, 65)
            self.assertIn("--allow-dirty", completed.stderr)
            self.assertFalse(archive.exists())

    def test_extracted_source_tamper_fails_before_native_build(self) -> None:
        if shutil.which("tar") is None or shutil.which("gzip") is None:
            self.skipTest("tar and gzip are required")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "source.tar.gz"
            packaged = subprocess.run(
                [str(PACKAGE_SCRIPT), "--allow-dirty", str(archive)],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                text=True, encoding="utf-8", check=False,
            )
            self.assertEqual(packaged.returncode, 0, msg=packaged.stderr)
            extracted = root / "extracted"
            extracted.mkdir()
            with tarfile.open(archive, "r:gz") as source_tar:
                source_tar.extractall(extracted)
            with (extracted / "FixtureApp.swift").open("a", encoding="utf-8") as stream:
                stream.write("// tampered\n")
            completed = subprocess.run(
                [
                    str(extracted / "run.sh"),
                    "--output", str(root / "output"),
                    "--build-dir", str(root / "build"),
                ],
                cwd=extracted, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                text=True, encoding="utf-8", check=False, timeout=30,
            )
            report = _decode_single_json(completed.stdout)
            self.assertEqual(completed.returncode, 1)
            self.assertEqual(report["error"]["code"], "invalid_source_provenance")
            self.assertFalse((root / "build").exists())


class MacOSTestkitCompileDiagnosticContracts(unittest.TestCase):
    @staticmethod
    def _copy_testkit(root: Path) -> Path:
        repository = root / "repository"
        copied = repository / "tests" / "macos"
        copied.parent.mkdir(parents=True)
        shutil.copytree(TESTKIT_ROOT, copied)
        git = shutil.which("git")
        if git is None:
            raise unittest.SkipTest("git is required")
        subprocess.run([git, "init", "-q"], cwd=repository, check=True)
        subprocess.run([git, "add", "tests/macos"], cwd=repository, check=True)
        subprocess.run(
            [
                git, "-c", "user.name=Test",
                "-c", "user.email=test@example.invalid",
                "commit", "-qm", "fixture baseline",
            ],
            cwd=repository,
            check=True,
        )
        # macOS exposes temporary directories through both /var and
        # /private/var.  build.sh intentionally canonicalizes its own path
        # with pwd -P, so the mock compiler must emit that same physical path
        # for the <TESTKIT> redaction contract to be deterministic.
        return copied.resolve()

    @staticmethod
    def _write_mock_tools(
        root: Path, testkit: Path, *, flood_output: bool = False
    ) -> Path:
        fake_bin = root / "bin"
        fake_bin.mkdir()
        flood_script = ""
        if flood_output:
            flood_script = """
    index=0
    while [ "$index" -lt 300 ]; do
      printf 'detail-%03d ' "$index" >&2
      printf '%0600d\n' 0 >&2
      index=$((index + 1))
    done
"""
        scripts = {
            "uname": """#!/bin/sh
if [ "${1:-}" = -s ]; then printf '%s\n' Darwin; else printf '%s\n' arm64; fi
""",
            "xcrun": f"""#!/bin/sh
case " $* " in
  *' --find swiftc '*) printf '%s\n' '{fake_bin / "swiftc"}' ;;
  *' --show-sdk-path '*) printf '%s\n' '{root / "Secret SDK"}' ;;
  *) exit 1 ;;
esac
""",
            "swiftc": f"""#!/bin/sh
if [ "${{1:-}}" = --version ]; then
  printf '%s\n' 'Apple Swift version 6.2'
  exit 0
fi
case " $* " in
  *FixtureApp.swift*)
    output=
    while [ "$#" -gt 0 ]; do
      if [ "$1" = -o ]; then shift; output=$1; break; fi
      shift
    done
    : >"$output"
    chmod 755 "$output"
    ;;
  *)
    printf '%s\n' '{testkit}/AXTestRunner.swift:139:9: error: synthetic failure' >&2
    printf '%s\n' '{root}/private-secret.txt:1:1: note: must be redacted' >&2
{flood_script}
    exit 1
    ;;
esac
""",
            "codesign": "#!/bin/sh\nexit 0\n",
        }
        for name, content in scripts.items():
            path = fake_bin / name
            path.write_text(content, encoding="utf-8")
            path.chmod(0o755)
        return fake_bin

    def test_runner_compile_failure_archives_only_bounded_sanitized_output(
        self,
    ) -> None:
        shell = shutil.which("sh")
        if shell is None or shutil.which("tar") is None:
            self.skipTest("POSIX sh and tar are required")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            testkit = self._copy_testkit(root)
            fake_bin = self._write_mock_tools(root, testkit)
            build_root = root / "private-build"
            output_root = root / "output"
            environment = os.environ.copy()
            environment["PATH"] = f"{fake_bin}:{environment['PATH']}"
            completed = subprocess.run(
                [
                    shell, str(testkit / "run.sh"),
                    "--output", str(output_root),
                    "--build-dir", str(build_root),
                ],
                cwd=testkit,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                errors="replace",
                env=environment,
                check=False,
                timeout=30,
            )
            self.assertEqual(completed.returncode, 1, msg=completed.stderr)
            report = _decode_single_json(completed.stdout)
            self.assertEqual(report["error"]["code"], "runner_compile_failed")
            self.assertIs(
                report["execution"]["compile_diagnostics_archived"], True
            )
            result_directory, = output_root.iterdir()
            archive_path = result_directory / "macos-ax-test-result.tar.gz"
            with tarfile.open(archive_path, "r:gz") as archive:
                self.assertEqual(
                    {member.name for member in archive.getmembers()},
                    ARCHIVE_MEMBERS,
                )
                readme_file = archive.extractfile("README.txt")
                self.assertIsNotNone(readme_file)
                assert readme_file is not None
                readme = readme_file.read().decode("utf-8")
            self.assertIn("synthetic failure", readme)
            self.assertIn("<TESTKIT>:AXTestRunner.swift:139:9", readme)
            self.assertIn("<ABSOLUTE_PATH>:1:1", readme)
            self.assertIn("raw_capture_truncated=false", readme)
            self.assertNotIn(str(testkit), readme)
            self.assertNotIn(str(root), readme)
            self.assertLessEqual(len(readme.encode("utf-8")), 16 * 1024)
            self.assertFalse((build_root / "compile-diagnostics.txt").exists())
            verified = subprocess.run(
                [str(TESTKIT_ROOT / "verify-result.py"), str(archive_path)],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                check=False,
                timeout=10,
            )
            verification = _decode_single_json(verified.stdout)
            self.assertNotEqual(verified.returncode, 0)
            self.assertIs(verification["archive_valid"], True)
            self.assertIs(verification["report_passed"], False)

    def test_excessive_compiler_output_is_drained_and_marked_truncated(
        self,
    ) -> None:
        shell = shutil.which("sh")
        if shell is None or shutil.which("tar") is None:
            self.skipTest("POSIX sh and tar are required")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            testkit = self._copy_testkit(root)
            fake_bin = self._write_mock_tools(
                root, testkit, flood_output=True
            )
            build_root = root / "private-build"
            output_root = root / "output"
            environment = os.environ.copy()
            environment["PATH"] = f"{fake_bin}:{environment['PATH']}"
            completed = subprocess.run(
                [
                    shell, str(testkit / "run.sh"),
                    "--output", str(output_root),
                    "--build-dir", str(build_root),
                ],
                cwd=testkit,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                errors="replace",
                env=environment,
                check=False,
                timeout=30,
            )
            self.assertEqual(completed.returncode, 1, msg=completed.stderr)
            report = _decode_single_json(completed.stdout)
            self.assertEqual(report["error"]["code"], "runner_compile_failed")
            result_directory, = output_root.iterdir()
            archive_path = result_directory / "macos-ax-test-result.tar.gz"
            with tarfile.open(archive_path, "r:gz") as archive:
                readme_file = archive.extractfile("README.txt")
                self.assertIsNotNone(readme_file)
                assert readme_file is not None
                readme = readme_file.read().decode("utf-8")
            self.assertIn("synthetic failure", readme)
            self.assertIn("raw_capture_limit_bytes=131072", readme)
            self.assertIn("raw_capture_truncated=true", readme)
            self.assertIn("[line truncated]", readme)
            self.assertIn("[compiler output truncated]", readme)
            self.assertNotIn(str(root), readme)
            self.assertLessEqual(len(readme.encode("utf-8")), 16 * 1024)
            self.assertFalse((build_root / "compile-diagnostics.txt").exists())


class NormalizedArchiveContracts(unittest.TestCase):
    @staticmethod
    def _create_source_files(source: Path) -> None:
        source.mkdir()
        (source / "plain.txt").write_text("plain\n", encoding="utf-8")
        (source / "tool.sh").write_text(
            "#!/bin/sh\nexit 0\n", encoding="utf-8"
        )
        (source / "plain.txt").chmod(0o666)
        (source / "tool.sh").chmod(0o711)
        os.utime(source / "plain.txt", (1_700_000_000, 1_700_000_000))
        os.utime(source / "tool.sh", (1_800_000_000, 1_800_000_000))

    def test_helper_produces_reproducible_normalized_archive(self) -> None:
        shell = shutil.which("sh")
        if shell is None or shutil.which("tar") is None:
            self.skipTest("POSIX sh and tar are required")
        if shutil.which("gzip") is None:
            self.skipTest("gzip is required")

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            self._create_source_files(source)
            archives = (root / "first.tar.gz", root / "second.tar.gz")
            command = (
                f'. "{ARCHIVE_SCRIPT}"; '
                'create_normalized_tar_gz "$1" "$2" '
                "plain.txt tool.sh"
            )
            for archive_path in archives:
                completed = subprocess.run(
                    [shell, "-c", command, "archive-test",
                     str(archive_path), str(source)],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    encoding="utf-8",
                    check=False,
                )
                self.assertEqual(
                    completed.returncode, 0, msg=completed.stderr
                )

            self.assertEqual(archives[0].read_bytes(), archives[1].read_bytes())
            gzip_header = archives[0].read_bytes()[:10]
            self.assertEqual(gzip_header[:3], b"\x1f\x8b\x08")
            self.assertEqual(gzip_header[3], 0)
            self.assertEqual(gzip_header[4:8], b"\0\0\0\0")
            self.assertEqual(gzip_header[8], 0)
            self.assertEqual(gzip_header[9], 3)
            self.assertEqual(int(archives[0].stat().st_mtime), NORMALIZED_MTIME)

            with tarfile.open(archives[0], "r:gz") as archive:
                members = archive.getmembers()
            self.assertEqual([member.name for member in members],
                             ["plain.txt", "tool.sh"])
            for member in members:
                with self.subTest(member=member.name):
                    self.assertTrue(member.isfile())
                    self.assertEqual(member.uid, 0)
                    self.assertEqual(member.gid, 0)
                    self.assertIn(member.uname, ("", "root"))
                    self.assertIn(member.gname, ("", "root"))
                    self.assertEqual(member.mtime, NORMALIZED_MTIME)
                    self.assertFalse(member.pax_headers)
            self.assertEqual(members[0].mode, 0o644)
            self.assertEqual(members[1].mode, 0o755)

    def test_helper_rejects_unknown_tar_without_publishing_archive(self) -> None:
        shell = shutil.which("sh")
        if shell is None:
            self.skipTest("a POSIX sh is unavailable")

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            self._create_source_files(source)
            fake_bin = root / "bin"
            fake_bin.mkdir()
            fake_tar = fake_bin / "tar"
            fake_tar.write_text(
                "#!/bin/sh\nprintf '%s\n' unknown-tar\n", encoding="utf-8"
            )
            fake_tar.chmod(0o755)
            archive_path = root / "must-not-exist.tar.gz"
            command = (
                f'. "{ARCHIVE_SCRIPT}"; '
                'create_normalized_tar_gz "$1" "$2" plain.txt'
            )
            environment = os.environ.copy()
            environment["PATH"] = f"{fake_bin}:{environment['PATH']}"
            completed = subprocess.run(
                [shell, "-c", command, "archive-test",
                 str(archive_path), str(source)],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                env=environment,
                check=False,
            )
            self.assertNotEqual(completed.returncode, 0)
            self.assertIn("仅支持已知", completed.stderr)
            self.assertFalse(archive_path.exists())

    def test_helper_uses_mac_os_bsdtar_compatible_arguments(self) -> None:
        shell = shutil.which("sh")
        if shell is None or shutil.which("gzip") is None:
            self.skipTest("POSIX sh and gzip are required")

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            self._create_source_files(source)
            fake_bin = root / "bin"
            fake_bin.mkdir()
            fake_tar = fake_bin / "tar"
            arguments = root / "arguments.txt"
            fake_tar.write_text(
                """#!/bin/sh
if [ "${1:-}" = --version ]; then
    printf '%s\n' 'bsdtar 3.5.3 - libarchive 3.5.3'
    exit 0
fi
: >"$MOCK_TAR_ARGUMENTS"
output=
while [ "$#" -gt 0 ]; do
    printf '%s\n' "$1" >>"$MOCK_TAR_ARGUMENTS"
    if [ "$1" = -cf ]; then
        shift
        output=$1
    fi
    shift
done
[ -n "$output" ] || exit 8
printf '%s' mocked-ustar >"$output"
""",
                encoding="utf-8",
            )
            fake_tar.chmod(0o755)
            archive_path = root / "mocked.tar.gz"
            command = (
                f'. "{ARCHIVE_SCRIPT}"; '
                'create_normalized_tar_gz "$1" "$2" plain.txt'
            )
            environment = os.environ.copy()
            environment["PATH"] = f"{fake_bin}:{environment['PATH']}"
            environment["MOCK_TAR_ARGUMENTS"] = str(arguments)
            completed = subprocess.run(
                [shell, "-c", command, "archive-test",
                 str(archive_path), str(source)],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                env=environment,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, msg=completed.stderr)
            passed_arguments = arguments.read_text(encoding="utf-8").splitlines()
            for argument in (
                "--format", "ustar", "--uid", "0", "--gid",
                "--uname", "root", "--gname", "--no-acls",
                "--no-fflags", "--no-xattrs", "--no-mac-metadata",
                "-cf", "-C", "plain.txt",
            ):
                with self.subTest(argument=argument):
                    self.assertIn(argument, passed_arguments)
            self.assertTrue(archive_path.is_file())


class IdentityAttestationContracts(unittest.TestCase):
    @staticmethod
    def _write_mock_tools(directory: Path) -> dict[str, Path]:
        tools: dict[str, Path] = {}
        scripts = {
            "swiftc": """#!/bin/sh
[ "${MOCK_FAIL_FIELD:-}" = swift ] && exit 9
[ "${MOCK_EMPTY_FIELD:-}" = swift ] || printf '%s\n' 'Apple Swift version 6.0'
""",
            "codesign": """#!/bin/sh
case " $* " in
    *' -r- '*)
        [ "${MOCK_FAIL_FIELD:-}" = requirement ] && exit 9
        [ "${MOCK_EMPTY_FIELD:-}" = requirement ] || printf '%s\n' 'designated => identifier test and anchor apple generic'
        ;;
    *' --verbose=4 '*)
        [ "${MOCK_FAIL_FIELD:-}" = details ] && exit 9
        case $* in
            *Runner*) identifier=dev.ai-auto-desktop.testkit.ax-runner ;;
            *) identifier=dev.ai-auto-desktop.testkit.fixture ;;
        esac
        [ "${MOCK_EMPTY_FIELD:-}" = identifier ] || printf 'Identifier=%s\n' "$identifier"
        printf '%s\n' 'TeamIdentifier=TESTTEAM'
        [ "${MOCK_EMPTY_FIELD:-}" = cdhash ] || printf '%s\n' 'CDHash=0123456789abcdef'
        ;;
    *) exit 8 ;;
esac
""",
            "lipo": """#!/bin/sh
[ "${MOCK_FAIL_FIELD:-}" = architectures ] && exit 9
[ "${MOCK_EMPTY_FIELD:-}" = architectures ] || printf '%s\n' arm64
""",
            "shasum": """#!/bin/sh
[ "${MOCK_FAIL_FIELD:-}" = sha256 ] && exit 9
[ "${MOCK_EMPTY_FIELD:-}" = sha256 ] || printf '%064d  executable\n' 0
""",
        }
        for name, content in scripts.items():
            path = directory / name
            path.write_text(content, encoding="utf-8")
            path.chmod(0o755)
            tools[name] = path
        return tools

    def _run_identity(
        self, root: Path, *, empty_field: str = "",
        fail_field: str = "", stability: str = "ephemeral"
    ) -> tuple[subprocess.CompletedProcess[str], Path]:
        shell = shutil.which("sh")
        assert shell is not None
        tool_dir = root / "tools"
        tool_dir.mkdir()
        tools = self._write_mock_tools(tool_dir)
        output = root / "identity.txt"
        command = (
            f'. "{IDENTITY_SCRIPT}"; '
            'write_identity_attestation "$1" "$2" "$3" "$4" '
            '"$5" "$6" "$7" clean "$8" Runner.app Runner.app/runner '
            'dev.ai-auto-desktop.testkit.ax-runner Fixture.app '
            'Fixture.app/fixture dev.ai-auto-desktop.testkit.fixture'
        )
        environment = os.environ.copy()
        environment["MOCK_EMPTY_FIELD"] = empty_field
        environment["MOCK_FAIL_FIELD"] = fail_field
        completed = subprocess.run(
            [shell, "-eu", "-c", command, "identity-test",
             str(output), str(tools["swiftc"]), str(tools["codesign"]),
             str(tools["lipo"]), str(tools["shasum"]), stability,
             "a" * 40, "b" * 64],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            env=environment,
            check=False,
        )
        return completed, output

    def test_complete_attestation_is_published(self) -> None:
        if shutil.which("sh") is None:
            self.skipTest("a POSIX sh is unavailable")
        with tempfile.TemporaryDirectory() as temporary:
            completed, output = self._run_identity(Path(temporary))
            self.assertEqual(completed.returncode, 0, msg=completed.stderr)
            attestation = output.read_text(encoding="utf-8")
            self.assertIn("swift=Apple Swift version 6.0", attestation)
            self.assertIn("identity_stability=ephemeral", attestation)
            self.assertIn(f"source_revision={'a' * 40}", attestation)
            self.assertIn(f"source_package_digest={'b' * 64}", attestation)
            for field in ("designated =>", "Identifier=", "CDHash=",
                          "architectures=", "sha256="):
                with self.subTest(field=field):
                    self.assertEqual(
                        sum(
                            line.startswith(field)
                            for line in attestation.splitlines()
                        ),
                        2,
                    )

    def test_empty_required_fields_fail_closed(self) -> None:
        if shutil.which("sh") is None:
            self.skipTest("a POSIX sh is unavailable")
        for field in ("swift", "requirement", "identifier", "cdhash",
                      "architectures", "sha256"):
            with self.subTest(field=field), tempfile.TemporaryDirectory(
            ) as temporary:
                completed, output = self._run_identity(
                    Path(temporary), empty_field=field
                )
                self.assertNotEqual(completed.returncode, 0)
                self.assertFalse(output.exists())
        with tempfile.TemporaryDirectory() as temporary:
            completed, output = self._run_identity(
                Path(temporary), stability=""
            )
            self.assertNotEqual(completed.returncode, 0)
            self.assertFalse(output.exists())

    def test_tool_failures_cannot_be_masked(self) -> None:
        if shutil.which("sh") is None:
            self.skipTest("a POSIX sh is unavailable")
        for field in ("swift", "requirement", "details",
                      "architectures", "sha256"):
            with self.subTest(field=field), tempfile.TemporaryDirectory(
            ) as temporary:
                completed, output = self._run_identity(
                    Path(temporary), fail_field=field
                )
                self.assertNotEqual(completed.returncode, 0)
                self.assertFalse(output.exists())


@unittest.skipIf(platform.system() == "Darwin", "exercises the non-macOS shell path")
class NonMacOSLauncherContracts(unittest.TestCase):
    def test_source_package_extracts_outside_repo_and_runs_directly(self) -> None:
        shell = shutil.which("sh")
        tar = shutil.which("tar")
        if shell is None or tar is None or shutil.which("gzip") is None:
            self.skipTest("POSIX sh, tar, and gzip are required")

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source_archive = root / "macos-testkit-source.tar.gz"
            packaged = subprocess.run(
                [str(PACKAGE_SCRIPT), "--allow-dirty", str(source_archive)],
                cwd=root,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                check=False,
            )
            self.assertEqual(packaged.returncode, 0, msg=packaged.stderr)
            with tarfile.open(source_archive, "r:gz") as archive:
                members = archive.getmembers()
            self.assertEqual(
                {member.name for member in members},
                SOURCE_PACKAGE_MEMBERS | {"SOURCE_MANIFEST.txt"},
            )
            self.assertEqual(len(members), len(SOURCE_PACKAGE_MEMBERS) + 1)
            self.assertTrue(all(member.isfile() for member in members))

            extracted = root / "extracted"
            extracted.mkdir()
            extraction = subprocess.run(
                [tar, "-xzf", str(source_archive), "-C", str(extracted)],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                check=False,
            )
            self.assertEqual(extraction.returncode, 0, msg=extraction.stderr)
            self.assertIn(
                'let typeTextSegments = ["ASCII ", "中文 ", "😀"]',
                _read(extracted / "AXTestRunner.swift"),
            )
            self.assertIn(
                'secureInput.stringValue = "fixture-secret"',
                _read(extracted / "FixtureApp.swift"),
            )
            source_manifest = _read(extracted / "SOURCE_MANIFEST.txt")
            self.assertRegex(
                source_manifest, r"(?m)^source_worktree=(?:clean|dirty)$"
            )
            self.assertIn("source_revision=", source_manifest)
            extracted_run = extracted / "run.sh"
            self.assertTrue(os.access(extracted_run, os.X_OK))
            completed = subprocess.run(
                [
                    str(extracted_run),
                    "--output", str(root / "output"),
                    "--build-dir", str(root / "build"),
                ],
                cwd=extracted,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                check=False,
                timeout=30,
            )
            self.assertEqual(completed.returncode, 3, msg=completed.stderr)
            report = _decode_single_json(completed.stdout)
            self.assertIsInstance(report, dict)
            assert isinstance(report, dict)
            self.assertEqual(report["message"], "requires_macos")
            self.assertEqual(report["error"]["code"], "requires_macos")
            self.assertEqual(report["execution"]["phase"], "build")
            self.assertEqual(report["execution"]["command_status"], 69)

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
                self.assertEqual(report["error"]["code"], "requires_macos")
                self.assertEqual(report["execution"]["phase"], "build")
                self.assertIs(report["execution"]["timed_out"], False)
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
