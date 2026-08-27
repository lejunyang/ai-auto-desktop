"""Security and packaging contracts for host-managed artifacts."""

from __future__ import annotations

import io
import json
import os
from pathlib import Path
import stat
import tempfile
import threading
import time
import unittest
from unittest import mock

import jsonschema
from PIL import Image

import ai_auto_desktop.artifacts as artifacts_module
from ai_auto_desktop.artifacts import (
    ARTIFACT_API_VERSION,
    ARTIFACT_KIND,
    ArtifactError,
    ArtifactRef,
    ArtifactStore,
)


PROJECT_ROOT = Path(__file__).resolve().parents[1]
CANONICAL_SCHEMA = (
    PROJECT_ROOT / "schemas" / "runtime" / "v1alpha1"
    / "artifact-ref.schema.json"
)
PACKAGED_SCHEMA = (
    PROJECT_ROOT / "src" / "ai_auto_desktop" / "schemas" / "runtime"
    / "v1alpha1" / "artifact-ref.schema.json"
)


def encoded_image(
    image_format: str = "PNG",
    *,
    size: tuple[int, int] = (3, 2),
    frames: int = 1,
) -> bytes:
    output = io.BytesIO()
    first = Image.new("RGB", size, "#235789")
    if frames == 1:
        first.save(output, format=image_format)
    else:
        remainder = [Image.new("RGB", size, "#f1d302") for _ in range(frames - 1)]
        first.save(
            output, format=image_format, save_all=True, append_images=remainder, loop=0
        )
    return output.getvalue()


def record_path(store: ArtifactStore, reference: ArtifactRef) -> Path:
    record = store._records[reference.artifact_id]
    return store._root / record.storage_name


class ArtifactRefTests(unittest.TestCase):
    def setUp(self) -> None:
        self.payload = encoded_image()
        self.reference = ArtifactRef(
            artifact_id="art_" + "a" * 32,
            digest="sha256:" + "b" * 64,
            media_type="image/png",
            size_bytes=len(self.payload),
        )

    def test_public_ref_is_strictly_closed_and_location_free(self) -> None:
        reference = self.reference
        document = reference.to_dict()

        self.assertEqual(
            set(document),
            {
                "apiVersion", "kind", "artifactId", "digest",
                "mediaType", "sizeBytes",
            },
        )
        self.assertEqual(document["apiVersion"], ARTIFACT_API_VERSION)
        self.assertEqual(document["kind"], ARTIFACT_KIND)
        self.assertNotIn("path", document)
        self.assertNotIn("runId", document)
        self.assertNotIn("storageKey", document)
        self.assertEqual(ArtifactRef.from_dict(document), reference)

        for mutation in (
            lambda value: value.__setitem__("path", "/tmp/leak"),
            lambda value: value.pop("digest"),
            lambda value: value.__setitem__("digest", "sha256:nope"),
            lambda value: value.__setitem__("sizeBytes", True),
            lambda value: value.__setitem__("mediaType", "text/plain"),
        ):
            invalid = dict(document)
            mutation(invalid)
            with self.subTest(invalid=invalid), self.assertRaises(ArtifactError) as raised:
                ArtifactRef.from_dict(invalid)
            self.assertEqual(raised.exception.code, "ARTIFACT.INVALID_REF")

    def test_schema_is_valid_mirrored_and_rejects_location_fields(self) -> None:
        self.assertEqual(CANONICAL_SCHEMA.read_bytes(), PACKAGED_SCHEMA.read_bytes())
        schema = json.loads(CANONICAL_SCHEMA.read_text(encoding="utf-8"))
        jsonschema.Draft202012Validator.check_schema(schema)
        document = self.reference.to_dict()
        validator = jsonschema.Draft202012Validator(schema)
        validator.validate(document)
        for forbidden in ("path", "runId", "storageKey"):
            invalid = {**document, forbidden: "secret"}
            self.assertFalse(validator.is_valid(invalid), forbidden)
        for field in ("artifactId", "digest"):
            for suffix in ("x", "\n"):
                invalid = {**document, field: document[field] + suffix}
                self.assertFalse(validator.is_valid(invalid), (field, suffix))
                with self.assertRaises(ArtifactError) as raised:
                    ArtifactRef.from_dict(invalid)
                self.assertEqual(raised.exception.code, "ARTIFACT.INVALID_REF")

    def test_ref_constants_require_exact_builtin_string_types(self) -> None:
        class EqualString(str):
            def __eq__(self, other: object) -> bool:
                return True

        document = self.reference.to_dict()
        for field in ("apiVersion", "kind", "artifactId", "digest", "mediaType"):
            invalid = {**document, field: EqualString(document[field])}
            with self.subTest(field=field), self.assertRaises(ArtifactError) as raised:
                ArtifactRef.from_dict(invalid)
            self.assertEqual(raised.exception.code, "ARTIFACT.INVALID_REF")


class ArtifactPlatformTests(unittest.TestCase):
    def test_non_posix_fails_before_creating_temporary_root(self) -> None:
        with mock.patch(
            "ai_auto_desktop.artifacts.os.name", "nt"
        ), mock.patch(
            "ai_auto_desktop.artifacts.tempfile.mkdtemp"
        ) as make_directory, self.assertRaises(ArtifactError) as raised:
            ArtifactStore()
        self.assertEqual(raised.exception.code, "ARTIFACT.PLATFORM_UNSUPPORTED")
        make_directory.assert_not_called()


@unittest.skipUnless(os.name == "posix", "ArtifactStore v1alpha1 is POSIX-only")
class ArtifactStoreTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.parent = Path(self.temporary.name)
        self.png = encoded_image()

    def assert_artifact_error(self, code: str, call) -> ArtifactError:
        with self.assertRaises(ArtifactError) as raised:
            call()
        self.assertEqual(raised.exception.code, code)
        serialized = json.dumps(raised.exception.to_dict())
        self.assertNotIn(os.fspath(self.parent), serialized)
        return raised.exception

    def test_scope_root_publish_and_path_free_readonly_handle(self) -> None:
        store = ArtifactStore(temporary_parent=self.parent)
        root = store._root
        if os.name == "posix":
            self.assertEqual(stat.S_IMODE(root.stat().st_mode), 0o700)
        reference = store.import_bytes(self.png, media_type="image/png")
        path = record_path(store, reference)
        info = path.stat()
        self.assertTrue(stat.S_ISREG(info.st_mode))
        self.assertEqual(info.st_nlink, 1)
        if os.name == "posix":
            self.assertEqual(stat.S_IMODE(info.st_mode), 0o400)
        self.assertNotIn(reference.artifact_id, path.name)
        self.assertNotIn(reference.digest.removeprefix("sha256:"), path.name)

        handle = store.resolve(reference)
        self.assertFalse(hasattr(handle, "name"))
        self.assertFalse(hasattr(handle, "fileno"))
        self.assertTrue(handle.readable())
        self.assertTrue(handle.seekable())
        self.assertEqual(handle.read(), self.png)
        with self.assertRaises((AttributeError, TypeError)):
            handle.write(b"x")
        handle.close()
        self.assertTrue(handle.closed)
        store.cleanup()
        self.assertTrue(root.exists())
        self.assertEqual(list(root.iterdir()), [])
        store.cleanup()
        self.assert_artifact_error("ARTIFACT.STORE_CLOSED", lambda: store.resolve(reference))

    def test_context_cleanup_closes_outstanding_handle(self) -> None:
        with ArtifactStore(temporary_parent=self.parent) as store:
            root = store._root
            reference = store.import_bytes(self.png)
            handle = store.resolve(reference)
        self.assertTrue(handle.closed)
        self.assertTrue(root.exists())
        self.assertEqual(list(root.iterdir()), [])

    def test_import_file_copies_source_and_accepts_external_hardlink(self) -> None:
        source = self.parent / "source.png"
        alias = self.parent / "alias.png"
        source.write_bytes(self.png)
        if os.name == "posix":
            os.link(source, alias)
            self.assertGreater(source.stat().st_nlink, 1)
        with ArtifactStore(temporary_parent=self.parent) as store:
            reference = store.import_file(source)
            source.write_bytes(encoded_image(size=(4, 4)))
            with store.resolve(reference) as handle:
                self.assertEqual(handle.read(), self.png)
            self.assertEqual(record_path(store, reference).stat().st_nlink, 1)

    @unittest.skipUnless(os.name == "posix", "O_NOFOLLOW is a POSIX boundary")
    def test_import_file_rejects_symlink_and_non_regular_source(self) -> None:
        target = self.parent / "target.png"
        target.write_bytes(self.png)
        link = self.parent / "link.png"
        link.symlink_to(target)
        with ArtifactStore(temporary_parent=self.parent) as store:
            self.assert_artifact_error(
                "ARTIFACT.INVALID_SOURCE", lambda: store.import_file(link)
            )
            self.assert_artifact_error(
                "ARTIFACT.INVALID_SOURCE", lambda: store.import_file(self.parent)
            )

    def test_import_file_uses_nonblocking_open_and_rejects_fifo_swap(self) -> None:
        fifo = self.parent / "fifo"
        os.mkfifo(fifo, 0o600)
        real_open = os.open
        observed_flags: list[int] = []

        def checked_open(path, flags, *args, **kwargs):
            if Path(path) == fifo:
                observed_flags.append(flags)
                if not flags & os.O_NONBLOCK:
                    raise AssertionError("FIFO open must be nonblocking")
            return real_open(path, flags, *args, **kwargs)

        with ArtifactStore(temporary_parent=self.parent) as store, mock.patch(
            "ai_auto_desktop.artifacts.os.open", side_effect=checked_open
        ):
            self.assert_artifact_error(
                "ARTIFACT.INVALID_SOURCE", lambda: store.import_file(fifo)
            )
        self.assertTrue(observed_flags)

    def test_import_source_failure_removes_all_staging_files(self) -> None:
        class FailingReader:
            def __init__(self) -> None:
                self.calls = 0

            def read(self, _size: int) -> bytes:
                self.calls += 1
                if self.calls == 1:
                    return b"\x89PNG\r\n\x1a\n"
                raise RuntimeError("source path /private/secret must not leak")

        with ArtifactStore(temporary_parent=self.parent) as store:
            self.assert_artifact_error(
                "ARTIFACT.SOURCE_READ_FAILED", lambda: store.import_source(FailingReader())
            )
            self.assertEqual(list(store._root.iterdir()), [])
            self.assertEqual(store._records, {})

        for invalid in (None, "text"):
            with self.subTest(invalid=invalid), ArtifactStore(
                temporary_parent=self.parent
            ) as store:
                source = mock.Mock()
                source.read.return_value = invalid
                self.assert_artifact_error(
                    "ARTIFACT.INVALID_SOURCE", lambda: store.import_source(source)
                )
                self.assertEqual(list(store._root.iterdir()), [])

    def test_source_cannot_return_more_than_the_requested_chunk(self) -> None:
        class OversizedReader:
            def read(self, size: int) -> bytes:
                return b"x" * (size + 1)

        with ArtifactStore(temporary_parent=self.parent) as store:
            self.assert_artifact_error(
                "ARTIFACT.INVALID_SOURCE",
                lambda: store.import_source(OversizedReader()),
            )
            self.assertEqual(list(store._root.iterdir()), [])

        class ExpandingBytes(bytes):
            def __bytes__(self) -> bytes:
                return b"x" * (2 * 1024 * 1024)

        class SubclassReader:
            def read(self, _size: int) -> bytes:
                return ExpandingBytes(b"x")

        with ArtifactStore(
            temporary_parent=self.parent,
            max_size_bytes=1024,
            max_total_bytes=1024,
        ) as store:
            self.assert_artifact_error(
                "ARTIFACT.INVALID_SOURCE",
                lambda: store.import_source(SubclassReader()),
            )
            self.assertEqual(list(store._root.iterdir()), [])

    def test_size_magic_declared_media_and_structural_limits(self) -> None:
        cases = (
            (ArtifactStore(max_size_bytes=8), self.png, None, "ARTIFACT.SIZE_LIMIT_EXCEEDED"),
            (ArtifactStore(), b"plain text", None, "ARTIFACT.UNSUPPORTED_MEDIA_TYPE"),
            (ArtifactStore(), self.png, "image/jpeg", "ARTIFACT.MEDIA_TYPE_MISMATCH"),
            (ArtifactStore(max_dimension=2), self.png, None, "ARTIFACT.DIMENSION_LIMIT_EXCEEDED"),
            (ArtifactStore(max_pixels=5), self.png, None, "ARTIFACT.PIXEL_LIMIT_EXCEEDED"),
            (ArtifactStore(), encoded_image("GIF", frames=2), None, "ARTIFACT.MULTI_FRAME_UNSUPPORTED"),
            (ArtifactStore(), self.png[:-8], None, "ARTIFACT.INVALID_IMAGE"),
        )
        for store, payload, media_type, code in cases:
            self.addCleanup(store.cleanup)
            with self.subTest(code=code):
                self.assert_artifact_error(
                    code, lambda s=store, p=payload, m=media_type: s.import_bytes(p, media_type=m)
                )
                self.assertEqual(list(store._root.iterdir()), [])
        with ArtifactStore(temporary_parent=self.parent) as store:
            self.assert_artifact_error(
                "ARTIFACT.UNSUPPORTED_MEDIA_TYPE",
                lambda: store.import_bytes(self.png, media_type=[]),
            )

    def test_artifact_count_and_total_storage_quotas_release_on_purge(self) -> None:
        now = [1.0]
        size = len(self.png)
        with ArtifactStore(
            temporary_parent=self.parent,
            max_artifacts=1,
            max_total_bytes=size,
            max_size_bytes=size,
            clock=lambda: now[0],
        ) as store:
            first = store.import_bytes(self.png, ttl_seconds=1)
            self.assert_artifact_error(
                "ARTIFACT.QUOTA_EXCEEDED", lambda: store.import_bytes(self.png)
            )
            self.assertEqual(len(store._records), 1)
            now[0] = 2.0
            self.assertEqual(store.purge_expired(), 1)
            self.assertNotIn(first.artifact_id, store._records)
            second = store.import_bytes(self.png)
            self.assertIn(second.artifact_id, store._records)

    def test_handle_count_and_resolved_byte_quotas_release_on_close(self) -> None:
        size = len(self.png)
        with ArtifactStore(
            temporary_parent=self.parent,
            max_open_handles=1,
            max_resolved_bytes=size,
        ) as store:
            reference = store.import_bytes(self.png)
            first = store.resolve(reference)
            self.assert_artifact_error(
                "ARTIFACT.QUOTA_EXCEEDED", lambda: store.resolve(reference)
            )
            first.close()
            with store.resolve(reference) as second:
                self.assertEqual(second.read(), self.png)

        with ArtifactStore(
            temporary_parent=self.parent,
            max_open_handles=2,
            max_resolved_bytes=size * 2 - 1,
        ) as store:
            reference = store.import_bytes(self.png)
            first = store.resolve(reference)
            self.assert_artifact_error(
                "ARTIFACT.QUOTA_EXCEEDED", lambda: store.resolve(reference)
            )
            first.close()

    def test_publish_failure_releases_all_quota_reservations(self) -> None:
        class FailingReader:
            def read(self, _size: int) -> bytes:
                raise RuntimeError("failed")

        with ArtifactStore(
            temporary_parent=self.parent, max_artifacts=1
        ) as store:
            self.assert_artifact_error(
                "ARTIFACT.SOURCE_READ_FAILED",
                lambda: store.import_source(FailingReader()),
            )
            reference = store.import_bytes(self.png)
            self.assertIn(reference.artifact_id, store._records)

    def test_quota_configuration_requires_positive_integers(self) -> None:
        for name in (
            "max_artifacts",
            "max_total_bytes",
            "max_open_handles",
            "max_resolved_bytes",
        ):
            for value in (0, -1, True, 1.5):
                with self.subTest(name=name, value=value), self.assertRaises(
                    ArtifactError
                ) as raised:
                    ArtifactStore(temporary_parent=self.parent, **{name: value})
                self.assertEqual(raised.exception.code, "ARTIFACT.INVALID_CONFIGURATION")

    def test_blocked_publish_does_not_block_resolve_or_purge(self) -> None:
        started = threading.Event()
        release = threading.Event()

        class BlockingReader:
            def __init__(self) -> None:
                self.sent = False

            def read(self, _size: int) -> bytes:
                if self.sent:
                    return b""
                started.set()
                release.wait(2)
                self.sent = True
                return self_payload

        self_payload = self.png
        with ArtifactStore(temporary_parent=self.parent) as store:
            existing = store.import_bytes(self.png)
            result: list[object] = []
            publisher = threading.Thread(
                target=lambda: result.append(store.import_source(BlockingReader()))
            )
            publisher.start()
            self.assertTrue(started.wait(1))
            before = time.monotonic()
            with store.resolve(existing) as handle:
                self.assertEqual(handle.read(), self.png)
            self.assertEqual(store.purge_expired(), 0)
            self.assertLess(time.monotonic() - before, 0.5)
            release.set()
            publisher.join(timeout=2)
            self.assertFalse(publisher.is_alive())
            self.assertEqual(len(result), 1)

    def test_blocked_source_does_not_hold_state_lock_and_cleanup_waits(self) -> None:
        started = threading.Event()
        release = threading.Event()

        class BlockingReader:
            def __init__(self) -> None:
                self.sent = False

            def read(self, _size: int) -> bytes:
                if self.sent:
                    return b""
                started.set()
                release.wait(2)
                self.sent = True
                return self_payload

        self_payload = self.png
        store = ArtifactStore(temporary_parent=self.parent)
        result: list[object] = []
        errors: list[ArtifactError] = []

        def publish() -> None:
            try:
                result.append(store.import_source(BlockingReader()))
            except ArtifactError as error:
                errors.append(error)

        publisher = threading.Thread(
            target=publish
        )
        publisher.start()
        self.assertTrue(started.wait(1))
        before = time.monotonic()
        self.assertEqual(store.purge_expired(), 0)
        self.assertLess(time.monotonic() - before, 0.5)
        cleaned = threading.Event()
        cleaner = threading.Thread(target=lambda: (store.cleanup(), cleaned.set()))
        cleaner.start()
        self.assertFalse(cleaned.wait(0.05))
        release.set()
        publisher.join(timeout=2)
        cleaner.join(timeout=2)
        self.assertFalse(publisher.is_alive())
        self.assertFalse(cleaner.is_alive())
        self.assertTrue(store.closed)
        self.assertEqual(result, [])
        self.assertEqual([error.code for error in errors], ["ARTIFACT.STORE_CLOSED"])

    def test_concurrent_cleanup_waits_for_physical_teardown(self) -> None:
        store = ArtifactStore(temporary_parent=self.parent)
        store.import_bytes(self.png)
        entered = threading.Event()
        release = threading.Event()
        finished: list[str] = []

        real_sync = artifacts_module._fsync_directory_fd

        def blocked_sync(descriptor: int) -> None:
            entered.set()
            release.wait(2)
            real_sync(descriptor)

        with mock.patch.object(
            artifacts_module, "_fsync_directory_fd", side_effect=blocked_sync
        ):
            first = threading.Thread(
                target=lambda: (store.cleanup(), finished.append("first"))
            )
            second = threading.Thread(
                target=lambda: (store.cleanup(), finished.append("second"))
            )
            first.start()
            self.assertTrue(entered.wait(1))
            second.start()
            second.join(timeout=0.05)
            self.assertTrue(second.is_alive())
            self.assertFalse(store.closed)
            release.set()
            first.join(timeout=2)
            second.join(timeout=2)

        self.assertFalse(first.is_alive())
        self.assertFalse(second.is_alive())
        self.assertTrue(store.closed)
        self.assertEqual(set(finished), {"first", "second"})
        self.assertEqual(store._root_fd, -1)

    def test_supported_single_frame_formats_are_detected(self) -> None:
        formats = {
            "PNG": "image/png",
            "JPEG": "image/jpeg",
            "GIF": "image/gif",
            "TIFF": "image/tiff",
            "BMP": "image/bmp",
            "PPM": "image/x-portable-anymap",
        }
        if "WEBP" in Image.registered_extensions().values():
            formats["WEBP"] = "image/webp"
        with ArtifactStore(temporary_parent=self.parent) as store:
            for image_format, media_type in formats.items():
                with self.subTest(image_format=image_format):
                    reference = store.import_bytes(encoded_image(image_format))
                    self.assertEqual(reference.media_type, media_type)

    def test_refs_are_execution_scoped_and_metadata_is_rechecked(self) -> None:
        with ArtifactStore(temporary_parent=self.parent) as first, ArtifactStore(
            temporary_parent=self.parent
        ) as second:
            reference = first.import_bytes(self.png)
            self.assert_artifact_error(
                "ARTIFACT.SCOPE_MISMATCH", lambda: second.resolve(reference)
            )
            tampered = {**reference.to_dict(), "sizeBytes": reference.size_bytes + 1}
            self.assert_artifact_error(
                "ARTIFACT.REF_MISMATCH", lambda: first.resolve(tampered)
            )

    def test_store_record_and_handle_metadata_do_not_alias_returned_ref(self) -> None:
        with ArtifactStore(temporary_parent=self.parent) as store:
            reference = store.import_bytes(self.png)
            original = reference.to_dict()
            object.__setattr__(reference, "media_type", "image/jpeg")
            self.assert_artifact_error(
                "ARTIFACT.REF_MISMATCH", lambda: store.resolve(reference)
            )
            handle = store.resolve(original)
            exposed = handle.reference
            object.__setattr__(exposed, "digest", "sha256:" + "0" * 64)
            self.assertEqual(handle.reference.to_dict(), original)
            self.assertEqual(handle.read(), self.png)
            handle.close()

    def test_expired_ref_is_rejected_without_opening_file(self) -> None:
        now = [100.0]
        with ArtifactStore(
            ttl_seconds=5, temporary_parent=self.parent, clock=lambda: now[0]
        ) as store:
            reference = store.import_bytes(self.png)
            now[0] = 105.0
            path = record_path(store, reference)
            self.assert_artifact_error(
                "ARTIFACT.EXPIRED", lambda: store.resolve(reference)
            )
            self.assertNotIn(reference.artifact_id, store._records)
            self.assertFalse(path.exists())

    def test_handle_expiry_is_enforced_by_every_read_position_operation(self) -> None:
        now = [10.0]
        with ArtifactStore(
            ttl_seconds=2, temporary_parent=self.parent, clock=lambda: now[0]
        ) as store:
            reference = store.import_bytes(self.png)
            handles = [store.resolve(reference) for _ in range(3)]
            now[0] = 12.0
            operations = (handles[0].read, handles[1].seek, handles[2].tell)
            for index, (handle, operation) in enumerate(zip(handles, operations)):
                with self.subTest(operation=operation), self.assertRaises(ArtifactError) as raised:
                    operation(0) if index == 1 else operation()
                self.assertEqual(raised.exception.code, "ARTIFACT.EXPIRED")
                self.assertTrue(handle.closed)
            self.assertNotIn(reference.artifact_id, store._records)

    def test_purge_expired_closes_all_handles_and_is_idempotent(self) -> None:
        now = [20.0]
        with ArtifactStore(
            ttl_seconds=3, temporary_parent=self.parent, clock=lambda: now[0]
        ) as store:
            first = store.import_bytes(self.png)
            first_handles = [store.resolve(first), store.resolve(first)]
            now[0] = 21.0
            live = store.import_bytes(self.png, ttl_seconds=10)
            live_handle = store.resolve(live)
            now[0] = 23.0
            self.assertEqual(store.purge_expired(), 1)
            self.assertTrue(all(handle.closed for handle in first_handles))
            self.assertFalse(live_handle.closed)
            self.assertEqual(store.purge_expired(), 0)
            live_handle.close()

    def test_handle_read_and_close_are_lock_serialized(self) -> None:
        with ArtifactStore(temporary_parent=self.parent) as store:
            handle = store.resolve(store.import_bytes(self.png))
            errors: list[BaseException] = []

            def reader() -> None:
                try:
                    for _ in range(100):
                        handle.seek(0)
                        handle.read(1)
                except (ValueError, ArtifactError):
                    pass
                except BaseException as error:
                    errors.append(error)

            thread = threading.Thread(target=reader)
            thread.start()
            handle.close()
            thread.join(timeout=2)
            self.assertFalse(thread.is_alive())
            self.assertEqual(errors, [])

    def test_invalid_clock_and_expiry_overflow_fail_closed(self) -> None:
        for clock in (None, lambda: -1, lambda: float("inf"), lambda: "0"):
            if clock is None:
                candidate: object = 1
            else:
                candidate = clock
            with self.subTest(clock=candidate), self.assertRaises(ArtifactError) as raised:
                ArtifactStore(temporary_parent=self.parent, clock=candidate)
            self.assertEqual(raised.exception.code, "ARTIFACT.INVALID_CONFIGURATION")

        with ArtifactStore(
            temporary_parent=self.parent, clock=lambda: 1e308
        ) as store:
            self.assert_artifact_error(
                "ARTIFACT.TTL_OVERFLOW",
                lambda: store.import_bytes(self.png, ttl_seconds=1e308),
            )

    def test_runtime_clock_failure_is_stable(self) -> None:
        values: list[object] = [1.0, 1.0, float("nan")]

        def clock() -> object:
            return values.pop(0)

        with ArtifactStore(temporary_parent=self.parent, clock=clock) as store:
            reference = store.import_bytes(self.png)
            self.assert_artifact_error(
                "ARTIFACT.CLOCK_INVALID", lambda: store.resolve(reference)
            )

    def test_content_mode_and_hardlink_tampering_are_rejected(self) -> None:
        if os.name != "posix":
            self.skipTest("POSIX inode and link semantics required")
        mutations = (
            lambda path: (path.chmod(0o600), path.write_bytes(encoded_image(size=(4, 4)))),
            lambda path: path.chmod(0o600),
            lambda path: os.link(path, self.parent / "escaped-link"),
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation), ArtifactStore(
                temporary_parent=self.parent
            ) as store:
                reference = store.import_bytes(self.png)
                mutation(record_path(store, reference))
                self.assert_artifact_error(
                    "ARTIFACT.INTEGRITY_FAILED", lambda: store.resolve(reference)
                )
            escaped = self.parent / "escaped-link"
            if escaped.exists():
                escaped.unlink()

    def test_symlink_and_rename_swap_are_rejected(self) -> None:
        if os.name != "posix":
            self.skipTest("POSIX symlink and inode semantics required")
        for replacement in ("symlink", "regular"):
            with self.subTest(replacement=replacement), ArtifactStore(
                temporary_parent=self.parent
            ) as store:
                reference = store.import_bytes(self.png)
                path = record_path(store, reference)
                path.chmod(0o600)
                path.unlink()
                if replacement == "symlink":
                    target = self.parent / "outside.png"
                    target.write_bytes(self.png)
                    path.symlink_to(target)
                else:
                    path.write_bytes(self.png)
                    path.chmod(0o400)
                self.assert_artifact_error(
                    "ARTIFACT.INTEGRITY_FAILED", lambda: store.resolve(reference)
                )

    def test_resolved_handle_survives_later_namespace_swap_safely(self) -> None:
        if os.name != "posix":
            self.skipTest("POSIX unlink semantics required")
        with ArtifactStore(temporary_parent=self.parent) as store:
            reference = store.import_bytes(self.png)
            handle = store.resolve(reference)
            path = record_path(store, reference)
            path.chmod(0o600)
            path.unlink()
            path.write_bytes(b"attacker")
            self.assertEqual(handle.read(), self.png)
            handle.close()
            self.assert_artifact_error(
                "ARTIFACT.INTEGRITY_FAILED", lambda: store.resolve(reference)
            )

    def test_resolved_handle_is_memory_snapshot_after_in_place_inode_mutation(self) -> None:
        with ArtifactStore(temporary_parent=self.parent) as store:
            reference = store.import_bytes(self.png)
            handle = store.resolve(reference)
            path = record_path(store, reference)
            path.chmod(0o600)
            path.write_bytes(encoded_image(size=(4, 4)))
            self.assertEqual(handle.read(), self.png)
            handle.close()
            self.assert_artifact_error(
                "ARTIFACT.INTEGRITY_FAILED", lambda: store.resolve(reference)
            )

    def test_store_root_replacement_is_rejected(self) -> None:
        if os.name != "posix":
            self.skipTest("POSIX rename semantics required")
        store = ArtifactStore(temporary_parent=self.parent)
        self.addCleanup(store.cleanup)
        reference = store.import_bytes(self.png)
        original = store._root.with_name(store._root.name + "-moved")
        store._root.rename(original)
        store._root.mkdir(mode=0o700)
        sentinel = store._root / "must-survive"
        sentinel.write_text("external", encoding="utf-8")
        replacement = store._root
        self.assertEqual(list(replacement.iterdir()), [sentinel])
        with store.resolve(reference) as handle:
            self.assertEqual(handle.read(), self.png)
        second = store.import_bytes(self.png)
        self.assertNotEqual(reference.artifact_id, second.artifact_id)
        self.assertEqual(list(replacement.iterdir()), [sentinel])
        store.cleanup()
        self.assertTrue(sentinel.exists(), "cleanup must not delete a replacement root")
        self.assertTrue(original.exists(), "renamed root namespace is never chased")
        self.assertEqual(list(original.iterdir()), [], "original root fd must be emptied")


if __name__ == "__main__":
    unittest.main()
