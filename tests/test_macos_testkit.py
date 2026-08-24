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
ARCHIVE_SCRIPT = TESTKIT_ROOT / "archive.sh"
IDENTITY_SCRIPT = TESTKIT_ROOT / "identity.sh"
PACKAGE_SCRIPT = TESTKIT_ROOT / "package-source.sh"
SOURCE_PACKAGE_MANIFEST = TESTKIT_ROOT / "SOURCE_PACKAGE_FILES.txt"
RUNNER_SOURCE = TESTKIT_ROOT / "AXTestRunner.swift"
FIXTURE_SOURCE = TESTKIT_ROOT / "FixtureApp.swift"
SHELL_SCRIPTS = (
    RUN_SCRIPT,
    BUILD_SCRIPT,
    ARCHIVE_SCRIPT,
    IDENTITY_SCRIPT,
    PACKAGE_SCRIPT,
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
                self.assertIn(marker, self.build + "\n" + self.identity)

    def test_launcher_uses_launch_services_and_report_as_source_of_truth(self) -> None:
        self.assertIn(
            '/usr/bin/open -n -W "$runner_app" --args', self.launcher
        )
        self.assertIn('--report "$temporary_report"', self.launcher)
        self.assertIn('--pid-file "$runner_pid_file"', self.launcher)
        self.assertIn('--cancel-file "$cancel_file"', self.launcher)
        self.assertIn('plutil -extract status raw', self.launcher)

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
            '"$5" "$6" Runner.app Runner.app/runner '
            'dev.ai-auto-desktop.testkit.ax-runner Fixture.app '
            'Fixture.app/fixture dev.ai-auto-desktop.testkit.fixture'
        )
        environment = os.environ.copy()
        environment["MOCK_EMPTY_FIELD"] = empty_field
        environment["MOCK_FAIL_FIELD"] = fail_field
        completed = subprocess.run(
            [shell, "-eu", "-c", command, "identity-test",
             str(output), str(tools["swiftc"]), str(tools["codesign"]),
             str(tools["lipo"]), str(tools["shasum"]), stability],
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
                [str(PACKAGE_SCRIPT), str(source_archive)],
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
            self.assertEqual({member.name for member in members},
                             SOURCE_PACKAGE_MEMBERS)
            self.assertEqual(len(members), len(SOURCE_PACKAGE_MEMBERS))
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
