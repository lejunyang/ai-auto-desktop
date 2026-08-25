"""Synthetic archive tests for the local macOS result verifier."""

from __future__ import annotations

import hashlib
import gzip
import io
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest


PROJECT_ROOT = Path(__file__).resolve().parents[1]
VERIFIER = PROJECT_ROOT / "tests" / "macos" / "verify-result.sh"
EXPECTED_MEMBERS = (
    "report.json",
    "README.txt",
    "identity.txt",
    "SHA256SUMS",
)
REQUIRED_CHECKS = (
    "screen_capture_preflight",
    "accessibility_trust",
    "bounded_discovery",
    "roles_and_ambiguity",
    "type_text_secure_rejected",
    "focus_and_reread",
    "set_value_and_reread",
    "type_text_unicode_and_reread",
    "press_and_reread",
    "pointer_click_and_reread",
)
SOURCE_REVISION = "a" * 40
SOURCE_PACKAGE_DIGEST = "b" * 64


def _report(status: str = "passed", *, with_source: bool = True) -> bytes:
    if status == "passed":
        checks = []
        for check_id in REQUIRED_CHECKS:
            check = {"id": check_id, "status": "pass", "message": "ok"}
            if check_id == "pointer_click_and_reread":
                check["evidence"] = {
                    "fresh_target": True,
                    "positive_area_bounds": True,
                    "bounds_ax_errors": [],
                    "target_pid_matches_fixture": True,
                    "pid_ax_error": 0,
                    "frontmost_before_dispatch": True,
                    "frontmost_at_dispatch": True,
                    "status_idle_before_dispatch": True,
                    "center_derived_from_ax_bounds": True,
                    "center_finite": True,
                    "hit_test_matches_target": True,
                    "hit_test_ax_error": 0,
                    "event_submitted": True,
                    "button": "left",
                    "position": "center",
                    "postcondition_reread": True,
                    "status_matches_from_fresh_snapshot": True,
                    "postcondition_ax_errors": [],
                }
            checks.append(check)
        message = "macOS AX fixture 测试通过"
    else:
        checks = [
            {
                "id": "fixture_launch",
                "status": "fail",
                "message": "failed",
            }
        ]
        message = "fixture launch failed"
    document = {
        "schema_version": "1.0",
        "kind": "macos_ax_fixture_test",
        "status": status,
        "message": message,
        "timestamp_utc": "2026-08-25T12:34:56Z",
        "platform": {
            "os": "macos",
            "architecture": "arm64",
            "rosetta_translated": False,
            "version": "macOS 15.6",
        },
        "identity": {
            "runner_bundle_id": "dev.ai-auto-desktop.testkit.ax-runner",
            "fixture_bundle_id": "dev.ai-auto-desktop.testkit.fixture",
            "launcher_declared_identity_stability": "ephemeral",
        },
        "permissions": {
            "accessibility": {
                "trusted": True,
                "prompt_requested": False,
            },
            "screen_capture": {
                "preflight_granted": False,
                "request_attempted": False,
                "capture_attempted": False,
            },
        },
        "limits": {
            "target_scope": "fixture_process_only",
            "screen_content_collected": False,
        },
        "checks": checks,
        "summary": {
            "passed": sum(
                check["status"] == "pass" for check in checks
            ),
            "failed": sum(
                check["status"] == "fail" for check in checks
            ),
            "total": len(checks),
        },
    }
    if with_source:
        document["source"] = {
            "revision": SOURCE_REVISION,
            "worktree": "clean",
            "package_digest": SOURCE_PACKAGE_DIGEST,
        }
    return (
        json.dumps(document, ensure_ascii=False, sort_keys=True).encode() + b"\n"
    )


def _identity(*, with_source: bool = True) -> bytes:
    provenance = (
        f"source_revision={SOURCE_REVISION}\n"
        "source_worktree=clean\n"
        f"source_package_digest={SOURCE_PACKAGE_DIGEST}\n"
        if with_source else ""
    )
    return (
        "swift=Apple Swift version 6.0\n"
        "identity_stability=ephemeral\n"
        + provenance
        +
        "[runner]\n"
        "designated => identifier runner and anchor apple generic\n"
        "Identifier=dev.ai-auto-desktop.testkit.ax-runner\n"
        "TeamIdentifier=TESTTEAM\n"
        "CDHash=0123456789abcdef0123456789abcdef01234567\n"
        "architectures=arm64\n"
        f"sha256={'1' * 64}\n"
        "[fixture]\n"
        "designated => identifier fixture and anchor apple generic\n"
        "Identifier=dev.ai-auto-desktop.testkit.fixture\n"
        "TeamIdentifier=TESTTEAM\n"
        "CDHash=89abcdef0123456789abcdef0123456789abcdef\n"
        "architectures=arm64\n"
        f"sha256={'2' * 64}\n"
    ).encode()


def _files(
    status: str = "passed", *, with_source: bool = True
) -> dict[str, bytes]:
    files = {
        "report.json": _report(status, with_source=with_source),
        "README.txt": "仅包含结构化测试结果。\n".encode(),
        "identity.txt": _identity(with_source=with_source),
    }
    files["SHA256SUMS"] = "".join(
        f"{hashlib.sha256(files[name]).hexdigest()}  {name}\n"
        for name in EXPECTED_MEMBERS[:3]
    ).encode()
    return files


def _refresh_manifest(files: dict[str, bytes]) -> None:
    files["SHA256SUMS"] = "".join(
        f"{hashlib.sha256(files[name]).hexdigest()}  {name}\n"
        for name in EXPECTED_MEMBERS[:3]
    ).encode()


def _minimal_unsupported_files() -> dict[str, bytes]:
    report = {
        "schema_version": "1.0",
        "kind": "macos_ax_fixture_test",
        "status": "unsupported",
        "message": "requires_macos",
        "platform": {"os": "Linux", "architecture": "unknown"},
        "permissions": {
            "accessibility": {"checked": False, "prompt_requested": False},
            "screen_capture": {
                "checked": False,
                "request_attempted": False,
                "capture_attempted": False,
            },
        },
        "checks": [],
        "summary": {"passed": 0, "failed": 0, "total": 0},
    }
    files = {
        "report.json": json.dumps(report, sort_keys=True).encode() + b"\n",
        "README.txt": b"result only\n",
        "identity.txt": b"identity_attestation=unavailable\n",
    }
    _refresh_manifest(files)
    return files


def _regular(name: str, content: bytes) -> tuple[tarfile.TarInfo, bytes]:
    member = tarfile.TarInfo(name)
    member.type = tarfile.REGTYPE
    member.mode = 0o644
    member.uid = member.gid = 0
    member.mtime = 946_684_800
    member.size = len(content)
    return member, content


def _write_archive(
    path: Path, entries: list[tuple[tarfile.TarInfo, bytes]],
    *,
    gzip_mtime: int = 0,
) -> None:
    tar_payload = io.BytesIO()
    with tarfile.open(
        fileobj=tar_payload, mode="w", format=tarfile.USTAR_FORMAT
    ) as archive:
        for member, content in entries:
            archive.addfile(
                member, io.BytesIO(content) if member.isreg() else None
            )
    with path.open("wb") as output:
        with gzip.GzipFile(
            filename="", mode="wb", fileobj=output, mtime=gzip_mtime,
            compresslevel=6,
        ) as stream:
            stream.write(tar_payload.getvalue())
    normalized = bytearray(path.read_bytes())
    normalized[9] = 3
    path.write_bytes(normalized)


def _regular_entries(
    files: dict[str, bytes],
) -> list[tuple[tarfile.TarInfo, bytes]]:
    return [_regular(name, files[name]) for name in EXPECTED_MEMBERS]


class MacOSResultVerifierTests(unittest.TestCase):
    def _run(
        self, archive: Path, expected_sha256: str | None = None,
        *, expected_revision: str | None = None,
        expected_package_digest: str | None = None,
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, object]]:
        arguments = [str(VERIFIER)]
        if expected_sha256 is not None:
            arguments.extend(["--expected-archive-sha256", expected_sha256])
        if expected_revision is not None:
            arguments.extend(["--expected-source-revision", expected_revision])
        if expected_package_digest is not None:
            arguments.extend([
                "--expected-source-package-digest", expected_package_digest
            ])
        arguments.append(str(archive))
        completed = subprocess.run(
            arguments,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            check=False,
            timeout=10,
        )
        self.assertEqual(completed.stderr, "")
        stripped = completed.stdout.strip()
        value, end = json.JSONDecoder().raw_decode(stripped)
        self.assertEqual(stripped[end:].strip(), "")
        self.assertIsInstance(value, dict)
        return completed, value

    def test_self_consistent_passed_archive_is_not_trusted_by_default(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "macos-ax-test-result.tar.gz"
            _write_archive(archive, _regular_entries(_files()))
            completed, result = self._run(archive)
        self.assertNotEqual(completed.returncode, 0)
        self.assertIs(result["archive_valid"], True)
        self.assertIs(result["verified_archive"], True)
        self.assertIs(result["report_passed"], True)
        self.assertIs(result["trusted_archive"], False)
        self.assertIs(result["qualified"], False)
        self.assertEqual(result["error"]["code"], "untrusted_archive")
        self.assertEqual(result["report"]["architecture"], "arm64")
        self.assertEqual(
            result["archive"]["members"], list(EXPECTED_MEMBERS)
        )

    def test_matching_independently_supplied_archive_hash_qualifies(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "macos-ax-test-result.tar.gz"
            _write_archive(archive, _regular_entries(_files()))
            expected = hashlib.sha256(archive.read_bytes()).hexdigest()
            completed, result = self._run(
                archive, expected, expected_revision=SOURCE_REVISION,
                expected_package_digest=SOURCE_PACKAGE_DIGEST,
            )
        self.assertEqual(completed.returncode, 0)
        self.assertIs(result["archive_valid"], True)
        self.assertIs(result["report_passed"], True)
        self.assertIs(result["trusted_archive"], True)
        self.assertIs(result["source_trusted"], True)
        self.assertIs(result["qualified"], True)

    def test_archive_hash_alone_no_longer_qualifies_source_bound_report(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "macos-ax-test-result.tar.gz"
            _write_archive(archive, _regular_entries(_files()))
            expected = hashlib.sha256(archive.read_bytes()).hexdigest()
            completed, result = self._run(archive, expected)
        self.assertNotEqual(completed.returncode, 0)
        self.assertIs(result["trusted_archive"], True)
        self.assertIs(result["source_trusted"], False)
        self.assertIs(result["qualified"], False)
        self.assertEqual(result["error"]["code"], "untrusted_source")

    def test_source_pins_cannot_self_authenticate_untrusted_archive(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "result.tar.gz"
            _write_archive(archive, _regular_entries(_files()))
            completed, result = self._run(
                archive, expected_revision=SOURCE_REVISION,
                expected_package_digest=SOURCE_PACKAGE_DIGEST,
            )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIs(result["trusted_archive"], False)
        self.assertIs(result["source_trusted"], False)
        self.assertIs(result["qualified"], False)
        self.assertEqual(result["error"]["code"], "untrusted_archive")

    def test_old_report_is_valid_but_fails_closed_for_source_trust(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "old-result.tar.gz"
            _write_archive(
                archive, _regular_entries(_files(with_source=False))
            )
            expected = hashlib.sha256(archive.read_bytes()).hexdigest()
            completed, result = self._run(
                archive, expected, expected_revision=SOURCE_REVISION,
                expected_package_digest=SOURCE_PACKAGE_DIGEST,
            )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIs(result["archive_valid"], True)
        self.assertIs(result["trusted_archive"], True)
        self.assertIs(result["source_trusted"], False)
        self.assertEqual(
            result["error"]["code"], "source_provenance_missing"
        )

    def test_source_revision_and_package_digest_mismatches_fail_closed(
        self,
    ) -> None:
        cases = (
            ("c" * 40, SOURCE_PACKAGE_DIGEST, "source_revision_mismatch"),
            (SOURCE_REVISION, "d" * 64, "source_package_digest_mismatch"),
        )
        for revision, digest, expected_code in cases:
            with self.subTest(expected_code=expected_code), tempfile.TemporaryDirectory(
            ) as temporary:
                archive = Path(temporary) / "result.tar.gz"
                _write_archive(archive, _regular_entries(_files()))
                archive_hash = hashlib.sha256(archive.read_bytes()).hexdigest()
                completed, result = self._run(
                    archive, archive_hash, expected_revision=revision,
                    expected_package_digest=digest,
                )
                self.assertNotEqual(completed.returncode, 0)
                self.assertIs(result["trusted_archive"], True)
                self.assertIs(result["source_trusted"], False)
                self.assertEqual(result["error"]["code"], expected_code)

    def test_report_identity_source_mismatch_invalidates_archive(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            files = _files()
            files["identity.txt"] = files["identity.txt"].replace(
                SOURCE_PACKAGE_DIGEST.encode(), b"c" * 64, 1
            )
            _refresh_manifest(files)
            archive = Path(temporary) / "result.tar.gz"
            _write_archive(archive, _regular_entries(files))
            completed, result = self._run(archive)
        self.assertNotEqual(completed.returncode, 0)
        self.assertIs(result["archive_valid"], False)
        self.assertEqual(
            result["error"]["code"], "source_provenance_mismatch"
        )

    def test_one_sided_source_provenance_invalidates_archive(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            files = _files()
            report = json.loads(files["report.json"])
            del report["source"]
            files["report.json"] = (
                json.dumps(report, sort_keys=True).encode() + b"\n"
            )
            _refresh_manifest(files)
            archive = Path(temporary) / "result.tar.gz"
            _write_archive(archive, _regular_entries(files))
            completed, result = self._run(archive)
        self.assertNotEqual(completed.returncode, 0)
        self.assertIs(result["archive_valid"], False)
        self.assertEqual(
            result["error"]["code"], "source_provenance_mismatch"
        )

    def test_dirty_source_never_qualifies(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            files = _files()
            files["report.json"] = files["report.json"].replace(
                b'"worktree": "clean"', b'"worktree": "dirty"'
            )
            files["identity.txt"] = files["identity.txt"].replace(
                b"source_worktree=clean", b"source_worktree=dirty"
            )
            _refresh_manifest(files)
            archive = Path(temporary) / "dirty-result.tar.gz"
            _write_archive(archive, _regular_entries(files))
            expected = hashlib.sha256(archive.read_bytes()).hexdigest()
            completed, result = self._run(
                archive, expected, expected_revision=SOURCE_REVISION,
                expected_package_digest=SOURCE_PACKAGE_DIGEST,
            )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIs(result["trusted_archive"], True)
        self.assertIs(result["source_trusted"], False)
        self.assertEqual(result["error"]["code"], "source_worktree_dirty")

    def test_source_pin_arguments_must_be_complete_and_well_formed(
        self,
    ) -> None:
        cases = (
            (SOURCE_REVISION, None, "incomplete_expected_source"),
            ("not-a-commit", SOURCE_PACKAGE_DIGEST,
             "invalid_expected_source_revision"),
            (SOURCE_REVISION, "ABC",
             "invalid_expected_source_package_digest"),
        )
        for revision, digest, expected_code in cases:
            with self.subTest(expected_code=expected_code), tempfile.TemporaryDirectory(
            ) as temporary:
                archive = Path(temporary) / "result.tar.gz"
                _write_archive(archive, _regular_entries(_files()))
                archive_hash = hashlib.sha256(archive.read_bytes()).hexdigest()
                completed, result = self._run(
                    archive, archive_hash, expected_revision=revision,
                    expected_package_digest=digest,
                )
                self.assertNotEqual(completed.returncode, 0)
                self.assertIs(result["trusted_archive"], True)
                self.assertIs(result["source_trusted"], False)
                self.assertEqual(result["error"]["code"], expected_code)

    def test_mismatched_expected_archive_hash_does_not_invalidate_archive(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "macos-ax-test-result.tar.gz"
            _write_archive(archive, _regular_entries(_files()))
            completed, result = self._run(archive, "f" * 64)
        self.assertNotEqual(completed.returncode, 0)
        self.assertIs(result["archive_valid"], True)
        self.assertIs(result["report_passed"], True)
        self.assertIs(result["trusted_archive"], False)
        self.assertEqual(
            result["error"]["code"], "archive_sha256_mismatch"
        )

    def test_tampered_member_is_rejected_by_sha256(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            files = _files()
            files["README.txt"] += b"tampered\n"
            archive = Path(temporary) / "macos-ax-test-result.tar.gz"
            _write_archive(archive, _regular_entries(files))
            completed, result = self._run(archive)
        self.assertNotEqual(completed.returncode, 0)
        self.assertIs(result["archive_valid"], False)
        self.assertIs(result["verified_archive"], False)
        self.assertEqual(result["error"]["code"], "hash_mismatch")

    def test_manifest_accepts_reordered_lines_and_one_or_more_spaces(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            files = _files()
            order = ("identity.txt", "report.json", "README.txt")
            separators = (" ", "   ", "\t")
            files["SHA256SUMS"] = "".join(
                f"{hashlib.sha256(files[name]).hexdigest()}{separator}{name}\n"
                for name, separator in zip(order, separators, strict=True)
            ).encode()
            archive = Path(temporary) / "macos-ax-test-result.tar.gz"
            _write_archive(archive, _regular_entries(files))
            expected = hashlib.sha256(archive.read_bytes()).hexdigest()
            completed, result = self._run(
                archive, expected, expected_revision=SOURCE_REVISION,
                expected_package_digest=SOURCE_PACKAGE_DIGEST,
            )
        self.assertEqual(completed.returncode, 0)
        self.assertIs(result["qualified"], True)

    def test_path_traversal_and_extra_member_are_rejected(self) -> None:
        cases = (
            ("../escape", "unsafe_member_path"),
            ("extra.txt", "extra_member"),
        )
        for name, expected_code in cases:
            with self.subTest(name=name), tempfile.TemporaryDirectory(
            ) as temporary:
                entries = _regular_entries(_files())
                entries.append(_regular(name, b"unexpected\n"))
                archive = Path(temporary) / "macos-ax-test-result.tar.gz"
                _write_archive(archive, entries)
                completed, result = self._run(archive)
                self.assertNotEqual(completed.returncode, 0)
                self.assertEqual(
                    result["error"]["code"], expected_code
                )

    def test_oversized_member_is_rejected_before_content_validation(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            files = _files()
            files["report.json"] = b"x" * (512 * 1024 + 1)
            archive = Path(temporary) / "macos-ax-test-result.tar.gz"
            _write_archive(archive, _regular_entries(files))
            completed, result = self._run(archive)
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(result["error"]["code"], "member_too_large")

    def test_gzip_bomb_and_truncated_archive_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bomb = root / "bomb.tar.gz"
            with bomb.open("wb") as output:
                with gzip.GzipFile(
                    filename="", mode="wb", fileobj=output, mtime=0,
                    compresslevel=6,
                ) as stream:
                    stream.write(b"x" * (4 * 1024 * 1024 + 1))
            normalized = bytearray(bomb.read_bytes())
            normalized[9] = 3
            bomb.write_bytes(normalized)
            completed, result = self._run(bomb)
            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(
                result["error"]["code"], "tar_payload_too_large"
            )

            archive = root / "valid.tar.gz"
            _write_archive(archive, _regular_entries(_files()))
            truncated = root / "truncated.tar.gz"
            truncated.write_bytes(archive.read_bytes()[:-8])
            completed, result = self._run(truncated)
            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(result["error"]["code"], "invalid_gzip")

    def test_non_normalized_gzip_and_tar_metadata_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "gzip-mtime.tar.gz"
            _write_archive(
                archive, _regular_entries(_files()), gzip_mtime=123
            )
            completed, result = self._run(archive)
            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(
                result["error"]["code"], "non_normalized_gzip"
            )

        mutations = (
            ("mode", 0o600),
            ("uid", 501),
            ("gid", 20),
            ("mtime", 1_700_000_000),
            ("uname", "alice"),
            ("gname", "staff"),
        )
        for field, value in mutations:
            with self.subTest(field=field), tempfile.TemporaryDirectory(
            ) as temporary:
                entries = _regular_entries(_files())
                setattr(entries[0][0], field, value)
                archive = Path(temporary) / "metadata.tar.gz"
                _write_archive(archive, entries)
                completed, result = self._run(archive)
                self.assertNotEqual(completed.returncode, 0)
                self.assertEqual(
                    result["error"]["code"],
                    "non_normalized_tar_metadata",
                )

    def test_gzip_header_and_single_member_contract_are_enforced(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            valid = root / "valid.tar.gz"
            _write_archive(valid, _regular_entries(_files()))

            for offset, value in ((8, 4), (9, 42)):
                with self.subTest(offset=offset):
                    payload = bytearray(valid.read_bytes())
                    payload[offset] = value
                    archive = root / f"header-{offset}.tar.gz"
                    archive.write_bytes(payload)
                    completed, result = self._run(archive)
                    self.assertNotEqual(completed.returncode, 0)
                    self.assertEqual(
                        result["error"]["code"],
                        "non_normalized_gzip",
                    )

            concatenated = root / "concatenated.tar.gz"
            concatenated.write_bytes(
                valid.read_bytes() + valid.read_bytes()
            )
            completed, result = self._run(concatenated)
            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(result["error"]["code"], "invalid_gzip")

    def test_noncanonical_member_order_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            files = _files()
            order = (
                "README.txt",
                "report.json",
                "identity.txt",
                "SHA256SUMS",
            )
            archive = Path(temporary) / "reordered.tar.gz"
            _write_archive(
                archive, [_regular(name, files[name]) for name in order]
            )
            completed, result = self._run(archive)
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(
            result["error"]["code"], "non_normalized_tar_order"
        )

    def test_links_devices_and_duplicate_members_are_rejected(self) -> None:
        special_types = (
            (tarfile.SYMTYPE, "symlink"),
            (tarfile.LNKTYPE, "hardlink"),
            (tarfile.CHRTYPE, "character_device"),
            (tarfile.BLKTYPE, "block_device"),
        )
        for member_type, expected_type in special_types:
            with self.subTest(
                member_type=expected_type
            ), tempfile.TemporaryDirectory() as temporary:
                files = _files()
                entries = [
                    _regular(name, files[name])
                    for name in EXPECTED_MEMBERS
                    if name != "README.txt"
                ]
                member = tarfile.TarInfo("README.txt")
                member.type = member_type
                member.linkname = "report.json"
                member.devmajor = member.devminor = 1
                entries.append((member, b""))
                archive = Path(temporary) / "macos-ax-test-result.tar.gz"
                _write_archive(archive, entries)
                completed, result = self._run(archive)
                self.assertNotEqual(completed.returncode, 0)
                self.assertEqual(
                    result["error"]["code"], "unsafe_member_type"
                )
                self.assertEqual(
                    result["error"]["details"]["type"],
                    expected_type,
                )

        with tempfile.TemporaryDirectory() as temporary:
            files = _files()
            entries = _regular_entries(files)
            entries.append(_regular("report.json", files["report.json"]))
            archive = Path(temporary) / "macos-ax-test-result.tar.gz"
            _write_archive(archive, entries)
            completed, result = self._run(archive)
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(result["error"]["code"], "duplicate_member")

        with tempfile.TemporaryDirectory() as temporary:
            files = _files()
            entries = [
                _regular(name, files[name])
                for name in EXPECTED_MEMBERS
                if name != "README.txt"
            ]
            archive = Path(temporary) / "macos-ax-test-result.tar.gz"
            _write_archive(archive, entries)
            completed, result = self._run(archive)
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(result["error"]["code"], "missing_member")

    def test_input_symlink_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "macos-ax-test-result.tar.gz"
            _write_archive(archive, _regular_entries(_files()))
            link = root / "returned.tar.gz"
            link.symlink_to(archive)
            completed, result = self._run(link)
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(result["error"]["code"], "input_symlink")

    def test_failed_report_is_verified_but_not_qualified(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "macos-ax-test-result.tar.gz"
            _write_archive(archive, _regular_entries(_files("failed")))
            completed, result = self._run(archive)
        self.assertNotEqual(completed.returncode, 0)
        self.assertIs(result["archive_valid"], True)
        self.assertIs(result["verified_archive"], True)
        self.assertIs(result["report_passed"], False)
        self.assertIs(result["trusted_archive"], False)
        self.assertIs(result["qualified"], False)
        self.assertEqual(result["report"]["status"], "failed")
        self.assertEqual(
            result["error"]["code"], "report_not_passed"
        )

    def test_failed_report_can_still_bind_trusted_source_for_diagnostics(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "failed-result.tar.gz"
            _write_archive(archive, _regular_entries(_files("failed")))
            expected = hashlib.sha256(archive.read_bytes()).hexdigest()
            completed, result = self._run(
                archive, expected, expected_revision=SOURCE_REVISION,
                expected_package_digest=SOURCE_PACKAGE_DIGEST,
            )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIs(result["archive_valid"], True)
        self.assertIs(result["trusted_archive"], True)
        self.assertIs(result["source_trusted"], True)
        self.assertIs(result["report_passed"], False)
        self.assertIs(result["qualified"], False)
        self.assertEqual(result["error"]["code"], "report_not_passed")

    def test_minimal_unsupported_report_is_verified_not_qualified(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "macos-ax-test-result.tar.gz"
            _write_archive(
                archive, _regular_entries(_minimal_unsupported_files())
            )
            completed, result = self._run(archive)
        self.assertNotEqual(completed.returncode, 0)
        self.assertIs(result["archive_valid"], True)
        self.assertIs(result["verified_archive"], True)
        self.assertIs(result["report_passed"], False)
        self.assertIs(result["qualified"], False)
        self.assertIs(result["identity"]["available"], False)
        self.assertEqual(result["report"]["status"], "unsupported")

    def test_launcher_timeout_diagnostics_are_validated(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            files = _minimal_unsupported_files()
            report = json.loads(files["report.json"])
            report["status"] = "failed"
            report["message"] = "runner_timeout"
            report["checks"] = [{
                "id": "launcher_runner",
                "status": "fail",
                "message": "runner_timeout",
            }]
            report["summary"] = {"passed": 0, "failed": 1, "total": 1}
            report["execution"] = {
                "phase": "runner",
                "command_status": 124,
                "timed_out": True,
                "timeout_seconds": 30,
                "runner_pid_observed": True,
            }
            report["error"] = {
                "code": "runner_timeout", "message": "runner_timeout"
            }
            files["report.json"] = (
                json.dumps(report, sort_keys=True).encode() + b"\n"
            )
            _refresh_manifest(files)
            archive = Path(temporary) / "timeout.tar.gz"
            _write_archive(archive, _regular_entries(files))
            completed, result = self._run(archive)
            self.assertNotEqual(completed.returncode, 0)
            self.assertIs(result["archive_valid"], True)
            self.assertEqual(result["error"]["code"], "report_not_passed")

            report["execution"]["timed_out"] = False
            files["report.json"] = (
                json.dumps(report, sort_keys=True).encode() + b"\n"
            )
            _refresh_manifest(files)
            invalid = Path(temporary) / "invalid-timeout.tar.gz"
            _write_archive(invalid, _regular_entries(files))
            completed, result = self._run(invalid)
        self.assertNotEqual(completed.returncode, 0)
        self.assertIs(result["archive_valid"], False)
        self.assertEqual(result["error"]["code"], "invalid_report_schema")

    def test_passed_report_rejects_invalid_identity_fields(self) -> None:
        mutations = (
            (
                "architectures=arm64",
                "architectures=mips",
                "invalid_identity_architecture",
            ),
            (
                "Identifier=dev.ai-auto-desktop.testkit.fixture",
                "Identifier=dev.example.fixture",
                "invalid_bundle_id",
            ),
            (f"sha256={'1' * 64}", "sha256=xyz", "invalid_executable_hash"),
        )
        for old, new, expected_code in mutations:
            with self.subTest(
                expected_code=expected_code
            ), tempfile.TemporaryDirectory() as temporary:
                files = _files()
                files["identity.txt"] = files["identity.txt"].replace(
                    old.encode(), new.encode(), 1
                )
                _refresh_manifest(files)
                archive = Path(temporary) / "macos-ax-test-result.tar.gz"
                _write_archive(archive, _regular_entries(files))
                completed, result = self._run(archive)
                self.assertNotEqual(completed.returncode, 0)
                self.assertEqual(
                    result["error"]["code"], expected_code
                )

    def test_passed_report_requires_all_checks_and_consistent_summary(
        self,
    ) -> None:
        mutations = (
            ("missing_required_checks", lambda report: report["checks"].pop()),
            (
                "invalid_report_summary",
                lambda report: report["summary"].__setitem__("total", 99),
            ),
        )
        for expected_code, mutate in mutations:
            with self.subTest(
                expected_code=expected_code
            ), tempfile.TemporaryDirectory() as temporary:
                files = _files()
                report = json.loads(files["report.json"])
                mutate(report)
                if expected_code == "missing_required_checks":
                    report["summary"]["passed"] -= 1
                    report["summary"]["total"] -= 1
                files["report.json"] = (
                    json.dumps(report, sort_keys=True).encode() + b"\n"
                )
                files["SHA256SUMS"] = "".join(
                    f"{hashlib.sha256(files[name]).hexdigest()}  {name}\n"
                    for name in EXPECTED_MEMBERS[:3]
                ).encode()
                archive = Path(temporary) / "macos-ax-test-result.tar.gz"
                _write_archive(archive, _regular_entries(files))
                completed, result = self._run(archive)
                self.assertNotEqual(completed.returncode, 0)
                self.assertEqual(
                    result["error"]["code"], expected_code
                )

    def test_passed_report_without_pointer_check_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            files = _files()
            report = json.loads(files["report.json"])
            report["checks"] = [
                check for check in report["checks"]
                if check["id"] != "pointer_click_and_reread"
            ]
            report["summary"]["passed"] -= 1
            report["summary"]["total"] -= 1
            files["report.json"] = (
                json.dumps(report, sort_keys=True).encode() + b"\n"
            )
            _refresh_manifest(files)
            archive = Path(temporary) / "macos-ax-test-result.tar.gz"
            _write_archive(archive, _regular_entries(files))
            completed, result = self._run(archive)
            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(
                result["error"]["code"], "missing_required_checks"
            )
            self.assertEqual(
                result["error"]["details"]["checks"],
                ["pointer_click_and_reread"],
            )

    def test_passed_report_rejects_failed_pointer_check(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            files = _files()
            report = json.loads(files["report.json"])
            pointer_check = next(
                check for check in report["checks"]
                if check["id"] == "pointer_click_and_reread"
            )
            pointer_check["status"] = "fail"
            report["summary"]["passed"] -= 1
            report["summary"]["failed"] += 1
            files["report.json"] = (
                json.dumps(report, sort_keys=True).encode() + b"\n"
            )
            _refresh_manifest(files)
            archive = Path(temporary) / "macos-ax-test-result.tar.gz"
            _write_archive(archive, _regular_entries(files))
            completed, result = self._run(archive)
            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(
                result["error"]["code"], "invalid_report_checks"
            )

    def test_passed_report_rejects_incomplete_pointer_evidence(self) -> None:
        mutations = (
            ("fresh_target", False),
            ("event_submitted", None),
            ("button", "right"),
            ("position", "top_left"),
            ("bounds_ax_errors", [1]),
            ("postcondition_ax_errors", "none"),
            ("pid_ax_error", 1),
        )
        for field, value in mutations:
            with self.subTest(field=field), tempfile.TemporaryDirectory() as temporary:
                files = _files()
                report = json.loads(files["report.json"])
                pointer_check = next(
                    check for check in report["checks"]
                    if check["id"] == "pointer_click_and_reread"
                )
                if value is None:
                    pointer_check["evidence"].pop(field)
                else:
                    pointer_check["evidence"][field] = value
                files["report.json"] = (
                    json.dumps(report, sort_keys=True).encode() + b"\n"
                )
                _refresh_manifest(files)
                archive = Path(temporary) / "macos-ax-test-result.tar.gz"
                _write_archive(archive, _regular_entries(files))
                completed, result = self._run(archive)
                self.assertNotEqual(completed.returncode, 0)
                self.assertEqual(
                    result["error"]["code"], "invalid_pointer_evidence"
                )


if __name__ == "__main__":
    unittest.main()
