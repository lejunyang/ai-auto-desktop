"""Run-scoped, host-managed immutable image artifacts.

``ArtifactRef`` is deliberately a capability-free transport value: it never
contains a filesystem path, run identifier, or storage key.  Only the
``ArtifactStore`` that issued a reference can resolve it.

The POSIX backend uses an fd-relative private directory.  Windows uses an
immutable, quota-bounded in-memory backend because v1alpha1 artifacts are
strictly run-scoped and are never restart-persistent.  Pillow decoding still
occurs in the Host process; untrusted image decoding must move to a separately
constrained worker before third-party producers can use this API.
"""

from __future__ import annotations

from dataclasses import dataclass
import errno
import hashlib
import hmac
import io
import math
import os
from pathlib import Path
import re
import secrets
import stat
import tempfile
import threading
import time
from typing import Any, BinaryIO, Callable, Mapping, TypeVar
import warnings
import weakref

from .errors import AutomationError

try:  # Fail closed at use time while keeping package imports diagnosable.
    from PIL import Image
except ImportError:  # pragma: no cover - exercised by dependency-failure tests
    Image = None  # type: ignore[assignment]


ARTIFACT_API_VERSION = "ai-auto-desktop.dev/v1alpha1"
ARTIFACT_KIND = "ArtifactRef"
DEFAULT_ARTIFACT_TTL_SECONDS = 3_600.0
DEFAULT_MAX_SIZE_BYTES = 64 * 1024 * 1024
DEFAULT_MAX_PIXELS = 16_000_000
DEFAULT_MAX_DIMENSION = 20_000
DEFAULT_MAX_ARTIFACTS = 128
DEFAULT_MAX_TOTAL_BYTES = 256 * 1024 * 1024
DEFAULT_MAX_OPEN_HANDLES = 64
DEFAULT_MAX_RESOLVED_BYTES = 128 * 1024 * 1024

SUPPORTED_MEDIA_TYPES = frozenset(
    {
        "image/png",
        "image/jpeg",
        "image/gif",
        "image/tiff",
        "image/bmp",
        "image/webp",
        "image/x-portable-anymap",
    }
)

_REF_FIELDS = frozenset(
    {
        "apiVersion",
        "kind",
        "artifactId",
        "digest",
        "mediaType",
        "sizeBytes",
    }
)
_ARTIFACT_ID = re.compile(r"art_[A-Za-z0-9_-]{32}\Z")
_DIGEST = re.compile(r"sha256:[0-9a-f]{64}\Z")
_STORAGE_NAME = re.compile(r"[0-9a-f]{64}\Z")
_CHUNK_SIZE = 1024 * 1024
_PUBLISHED_MODE = stat.S_IRUSR
_DIRECTORY_FLAGS = (
    os.O_RDONLY
    | getattr(os, "O_CLOEXEC", 0)
    | getattr(os, "O_DIRECTORY", 0)
    | getattr(os, "O_NOFOLLOW", 0)
)
_READ_FLAGS = (
    os.O_RDONLY
    | getattr(os, "O_CLOEXEC", 0)
    | getattr(os, "O_NOFOLLOW", 0)
    | getattr(os, "O_NONBLOCK", 0)
)
_T = TypeVar("_T")


class ArtifactError(AutomationError):
    """Stable, path-redacted artifact boundary error."""

    def __init__(
        self,
        code: str,
        message: str,
        *,
        details: Mapping[str, Any] | None = None,
    ) -> None:
        super().__init__(
            code,
            message,
            category="artifact",
            phase="artifact",
            details=details,
        )


@dataclass(frozen=True, slots=True)
class ArtifactRef:
    """Closed public description of an artifact managed by a Host store."""

    artifact_id: str
    digest: str
    media_type: str
    size_bytes: int
    api_version: str = ARTIFACT_API_VERSION
    kind: str = ARTIFACT_KIND

    def __post_init__(self) -> None:
        _validate_ref_values(
            self.api_version,
            self.kind,
            self.artifact_id,
            self.digest,
            self.media_type,
            self.size_bytes,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "apiVersion": self.api_version,
            "kind": self.kind,
            "artifactId": self.artifact_id,
            "digest": self.digest,
            "mediaType": self.media_type,
            "sizeBytes": self.size_bytes,
        }

    @classmethod
    def from_dict(cls, value: Mapping[str, Any]) -> "ArtifactRef":
        try:
            data = dict(value)
        except Exception:
            _invalid_ref()
        if frozenset(data) != _REF_FIELDS:
            _invalid_ref()
        return cls(
            api_version=data["apiVersion"],
            kind=data["kind"],
            artifact_id=data["artifactId"],
            digest=data["digest"],
            media_type=data["mediaType"],
            size_bytes=data["sizeBytes"],
        )


def _copy_ref(reference: ArtifactRef) -> ArtifactRef:
    return ArtifactRef(
        api_version=reference.api_version,
        kind=reference.kind,
        artifact_id=reference.artifact_id,
        digest=reference.digest,
        media_type=reference.media_type,
        size_bytes=reference.size_bytes,
    )


class ArtifactHandle:
    """A path-free, expiring read handle over an attested bytes snapshot."""

    __slots__ = (
        "_artifact_id",
        "_clock",
        "_expired",
        "_expires_at",
        "_lock",
        "_on_close",
        "_on_expire",
        "_reference",
        "_size_bytes",
        "_stream",
        "__weakref__",
    )

    def __init__(
        self,
        payload: bytes,
        reference: ArtifactRef,
        *,
        expires_at: float,
        clock: Callable[[], float],
        on_close: Callable[["ArtifactHandle"], None],
        on_expire: Callable[["ArtifactHandle"], None],
    ) -> None:
        self._artifact_id = reference.artifact_id
        self._size_bytes = reference.size_bytes
        self._reference = _copy_ref(reference)
        self._stream = io.BytesIO(bytes(payload))
        self._expires_at = expires_at
        self._clock = clock
        self._on_close: Callable[["ArtifactHandle"], None] | None = on_close
        self._on_expire: Callable[["ArtifactHandle"], None] | None = on_expire
        self._expired = False
        self._lock = threading.RLock()

    @property
    def reference(self) -> ArtifactRef:
        return _copy_ref(self._reference)

    @property
    def closed(self) -> bool:
        with self._lock:
            return self._stream.closed

    def readable(self) -> bool:
        return self._operate(self._stream.readable)

    def seekable(self) -> bool:
        return self._operate(self._stream.seekable)

    def tell(self) -> int:
        return self._operate(self._stream.tell)

    def seek(self, offset: int, whence: int = os.SEEK_SET) -> int:
        return self._operate(lambda: self._stream.seek(offset, whence))

    def read(self, size: int = -1) -> bytes:
        return self._operate(lambda: self._stream.read(size))

    def readinto(self, buffer: Any) -> int | None:
        return self._operate(lambda: self._stream.readinto(buffer))

    def close(self) -> None:
        callback: Callable[["ArtifactHandle"], None] | None
        with self._lock:
            callback = self._on_close
            self._on_close = None
            self._on_expire = None
            self._stream.close()
        if callback is not None:
            callback(self)

    def __enter__(self) -> "ArtifactHandle":
        self._operate(lambda: None)
        return self

    def __exit__(self, exc_type: object, exc: object, traceback: object) -> None:
        self.close()

    def __del__(self) -> None:
        try:
            self.close()
        except Exception:
            pass

    def _operate(self, operation: Callable[[], _T]) -> _T:
        callback: Callable[["ArtifactHandle"], None] | None = None
        expired = False
        with self._lock:
            if self._expired:
                expired = True
            else:
                now = _read_clock(self._clock, initial=False)
                if now >= self._expires_at:
                    self._expired = True
                    expired = True
                    callback = self._on_expire
                    self._on_expire = None
                    self._on_close = None
                    self._stream.close()
            if not expired:
                return operation()
        if callback is not None:
            callback(self)
        raise ArtifactError(
            "ARTIFACT.EXPIRED",
            "Artifact handle has expired.",
        )

    def _expire_from_store(self) -> None:
        with self._lock:
            self._expired = True
            self._on_close = None
            self._on_expire = None
            self._stream.close()

    def _close_from_store(self) -> None:
        with self._lock:
            self._on_close = None
            self._on_expire = None
            self._stream.close()


@dataclass(frozen=True, slots=True)
class _ArtifactRecord:
    ref: ArtifactRef
    storage_name: str
    expires_at: float
    device: int
    inode: int
    owner: int
    mode: int
    links: int
    modified_ns: int
    changed_ns: int
    payload: bytes | None = None


@dataclass(slots=True)
class _PublishReservation:
    size_bytes: int
    active: bool = True


@dataclass(slots=True)
class _ResolveReservation:
    artifact_id: str
    size_bytes: int
    active: bool = True


@dataclass(frozen=True, slots=True)
class _PublishedArtifact:
    storage_name: str
    digest: str
    media_type: str
    size_bytes: int
    info: os.stat_result | None
    payload: bytes | None = None


class ArtifactBatch:
    """Host-private, all-or-nothing publication batch.

    Payloads are fully copied and validated while staging, but their public
    references cannot be resolved until :meth:`commit` publishes the entire
    batch under the store lock.  Closing an uncommitted batch removes every
    staged leaf.
    """

    __slots__ = ("_closed", "_entries", "_lock", "_store")

    def __init__(self, store: "ArtifactStore") -> None:
        self._store = store
        self._entries: list[
            tuple[ArtifactRef, _ArtifactRecord, _PublishReservation]
        ] = []
        self._closed = False
        self._lock = threading.RLock()

    def import_bytes(
        self,
        data: bytes | bytearray | memoryview,
        *,
        media_type: str | None = None,
        ttl_seconds: float | None = None,
    ) -> ArtifactRef:
        if not isinstance(data, (bytes, bytearray, memoryview)):
            raise ArtifactError(
                "ARTIFACT.INVALID_SOURCE",
                "Artifact bytes must be a bytes-like value.",
            )
        if len(data) > self._store._max_size_bytes:
            raise ArtifactError(
                "ARTIFACT.SIZE_LIMIT_EXCEEDED",
                "Artifact exceeds the configured byte limit.",
                details={"maxSizeBytes": self._store._max_size_bytes},
            )
        return self.import_source(
            io.BytesIO(bytes(data)),
            media_type=media_type,
            ttl_seconds=ttl_seconds,
        )

    def import_source(
        self,
        source: BinaryIO,
        *,
        media_type: str | None = None,
        ttl_seconds: float | None = None,
    ) -> ArtifactRef:
        with self._lock:
            if self._closed:
                raise ArtifactError(
                    "ARTIFACT.BATCH_CLOSED", "Artifact publication batch is closed."
                )
            return self._store._stage_batch_source(
                self, source, media_type=media_type, ttl_seconds=ttl_seconds
            )

    def commit(self) -> tuple[ArtifactRef, ...]:
        with self._lock:
            if self._closed:
                raise ArtifactError(
                    "ARTIFACT.BATCH_CLOSED", "Artifact publication batch is closed."
                )
            try:
                references = self._store._commit_batch(self)
            except BaseException:
                self._store._rollback_batch(self)
                self._closed = True
                raise
            self._closed = True
            return references

    def rollback(self) -> None:
        with self._lock:
            if self._closed:
                return
            self._store._rollback_batch(self)
            self._closed = True

    close = rollback

    def __enter__(self) -> "ArtifactBatch":
        return self

    def __exit__(self, exc_type: object, exc: object, traceback: object) -> None:
        self.rollback()


class ArtifactStore:
    """An isolated execution scope for immutable image artifacts."""

    def __init__(
        self,
        *,
        ttl_seconds: float = DEFAULT_ARTIFACT_TTL_SECONDS,
        max_size_bytes: int = DEFAULT_MAX_SIZE_BYTES,
        max_pixels: int = DEFAULT_MAX_PIXELS,
        max_dimension: int = DEFAULT_MAX_DIMENSION,
        max_artifacts: int = DEFAULT_MAX_ARTIFACTS,
        max_total_bytes: int = DEFAULT_MAX_TOTAL_BYTES,
        max_open_handles: int = DEFAULT_MAX_OPEN_HANDLES,
        max_resolved_bytes: int = DEFAULT_MAX_RESOLVED_BYTES,
        temporary_parent: str | os.PathLike[str] | None = None,
        clock: Callable[[], float] | None = None,
    ) -> None:
        _require_supported_platform()
        self._ttl_seconds = _positive_number(ttl_seconds, "ttl_seconds")
        self._max_size_bytes = _positive_integer(max_size_bytes, "max_size_bytes")
        self._max_pixels = _positive_integer(max_pixels, "max_pixels")
        self._max_dimension = _positive_integer(max_dimension, "max_dimension")
        self._max_artifacts = _positive_integer(max_artifacts, "max_artifacts")
        self._max_total_bytes = _positive_integer(
            max_total_bytes, "max_total_bytes"
        )
        self._max_open_handles = _positive_integer(
            max_open_handles, "max_open_handles"
        )
        self._max_resolved_bytes = _positive_integer(
            max_resolved_bytes, "max_resolved_bytes"
        )
        if clock is None:
            clock = time.monotonic
        if not callable(clock):
            raise ArtifactError(
                "ARTIFACT.INVALID_CONFIGURATION",
                "clock must be callable.",
            )
        _read_clock(clock, initial=True)
        self._clock = clock
        self._lock = threading.RLock()
        self._condition = threading.Condition(self._lock)
        self._publish_lock = threading.Lock()
        self._records: dict[str, _ArtifactRecord] = {}
        self._active_batches: set[ArtifactBatch] = set()
        self._provisional_ids: set[str] = set()
        self._handles: weakref.WeakSet[ArtifactHandle] = weakref.WeakSet()
        self._temporary_names: set[str] = set()
        self._active_operations = 0
        self._closing = False
        self._closed = False
        self._stored_bytes = 0
        self._reserved_artifacts = 0
        self._reserved_storage_bytes = 0
        self._open_handle_count = 0
        self._resolved_bytes = 0
        self._reserved_handles = 0
        self._reserved_resolved_bytes = 0
        self._memory_backend = os.name == "nt"
        if self._memory_backend and temporary_parent is not None:
            raise ArtifactError(
                "ARTIFACT.INVALID_CONFIGURATION",
                "temporary_parent is unavailable for the Windows memory backend.",
            )
        self._root_fd = -1
        self._parent_fd = -1
        self._root: Path | None = None
        self._root_name = ""
        root: Path | None = None
        root_fd = -1
        parent_fd = -1
        created_info: os.stat_result | None = None
        root_info: os.stat_result | None = None
        if self._memory_backend:
            return
        try:
            root = Path(
                tempfile.mkdtemp(
                    prefix="aad-artifact-",
                    dir=os.fspath(temporary_parent) if temporary_parent is not None else None,
                )
            )
            root_name = root.name
            if not root_name or root_name in {".", ".."} or os.sep in root_name:
                raise OSError(errno.EINVAL, "unsafe root name")
            parent_fd = os.open(root.parent, _DIRECTORY_FLAGS)
            created_info = os.stat(
                root_name, dir_fd=parent_fd, follow_symlinks=False
            )
            root_fd = os.open(root_name, _DIRECTORY_FLAGS, dir_fd=parent_fd)
            os.fchmod(root_fd, 0o700)
            root_info = os.fstat(root_fd)
            if not _same_inode(created_info, root_info):
                raise OSError(errno.ESTALE, "root changed during initialization")
            _validate_root_stat(root_info)
        except (OSError, TypeError, ValueError):
            _close_fd(root_fd)
            if parent_fd >= 0 and root is not None and created_info is not None:
                _rmdir_if_same(parent_fd, root.name, created_info)
            _close_fd(parent_fd)
            raise ArtifactError(
                "ARTIFACT.STORE_UNAVAILABLE",
                "Artifact storage could not be initialized securely.",
            ) from None
        self._root = root
        self._root_name = root.name
        self._root_fd = root_fd
        self._parent_fd = parent_fd
        self._root_device = root_info.st_dev
        self._root_inode = root_info.st_ino
        self._root_owner = root_info.st_uid

    @property
    def closed(self) -> bool:
        with self._lock:
            return self._closed

    def __enter__(self) -> "ArtifactStore":
        with self._lock:
            self._ensure_open()
        return self

    def __exit__(self, exc_type: object, exc: object, traceback: object) -> None:
        self.cleanup()

    def __del__(self) -> None:
        try:
            self.cleanup()
        except Exception:
            pass

    def batch(self) -> ArtifactBatch:
        """Create a Host-private atomic publication batch."""

        with self._lock:
            self._ensure_open()
            batch = ArtifactBatch(self)
            self._active_batches.add(batch)
            return batch

    def import_bytes(
        self,
        data: bytes | bytearray | memoryview,
        *,
        media_type: str | None = None,
        ttl_seconds: float | None = None,
    ) -> ArtifactRef:
        if not isinstance(data, (bytes, bytearray, memoryview)):
            raise ArtifactError(
                "ARTIFACT.INVALID_SOURCE",
                "Artifact bytes must be a bytes-like value.",
            )
        if len(data) > self._max_size_bytes:
            raise ArtifactError(
                "ARTIFACT.SIZE_LIMIT_EXCEEDED",
                "Artifact exceeds the configured byte limit.",
                details={"maxSizeBytes": self._max_size_bytes},
            )
        return self.import_source(
            io.BytesIO(bytes(data)),
            media_type=media_type,
            ttl_seconds=ttl_seconds,
        )

    def put_bytes(
        self,
        data: bytes | bytearray | memoryview,
        *,
        media_type: str | None = None,
        ttl_seconds: float | None = None,
    ) -> ArtifactRef:
        return self.import_bytes(data, media_type=media_type, ttl_seconds=ttl_seconds)

    def import_file(
        self,
        source: str | os.PathLike[str],
        *,
        media_type: str | None = None,
        ttl_seconds: float | None = None,
    ) -> ArtifactRef:
        if self._memory_backend:
            raise ArtifactError(
                "ARTIFACT.PLATFORM_UNSUPPORTED",
                "Windows ArtifactStore accepts trusted bytes/readers, not paths.",
            )
        try:
            source_path = Path(source)
            descriptor = os.open(source_path, _READ_FLAGS)
        except (OSError, TypeError, ValueError):
            raise ArtifactError(
                "ARTIFACT.INVALID_SOURCE",
                "Artifact source could not be opened safely.",
            ) from None
        try:
            opened = os.fstat(descriptor)
            if not stat.S_ISREG(opened.st_mode):
                raise ArtifactError(
                    "ARTIFACT.INVALID_SOURCE",
                    "Artifact source must be a regular file.",
                )
            with os.fdopen(descriptor, "rb", closefd=True) as reader:
                descriptor = -1
                reference = self.import_source(
                    reader, media_type=media_type, ttl_seconds=ttl_seconds
                )
                finished = os.fstat(reader.fileno())
                if not _stable_source(opened, finished):
                    with self._lock:
                        self._discard_locked(reference.artifact_id)
                    raise ArtifactError(
                        "ARTIFACT.SOURCE_CHANGED",
                        "Artifact source changed while it was copied.",
                    )
                return reference
        finally:
            _close_fd(descriptor)

    def import_path(
        self,
        source: str | os.PathLike[str],
        *,
        media_type: str | None = None,
        ttl_seconds: float | None = None,
    ) -> ArtifactRef:
        return self.import_file(source, media_type=media_type, ttl_seconds=ttl_seconds)

    def import_source(
        self,
        source: BinaryIO,
        *,
        media_type: str | None = None,
        ttl_seconds: float | None = None,
    ) -> ArtifactRef:
        if not callable(getattr(source, "read", None)):
            raise ArtifactError(
                "ARTIFACT.INVALID_SOURCE",
                "Artifact source must be a binary reader.",
            )
        ttl = self._ttl_seconds if ttl_seconds is None else _positive_number(
            ttl_seconds, "ttl_seconds"
        )
        reservation: _PublishReservation | None = None
        operation_active = False
        with self._publish_lock:
            try:
                with self._lock:
                    self._ensure_open()
                    self._validate_root_fd()
                    now = _read_clock(self._clock, initial=False)
                    expires_at = now + ttl
                    if not math.isfinite(expires_at):
                        raise ArtifactError(
                            "ARTIFACT.TTL_OVERFLOW",
                            "Artifact expiry exceeds the supported clock range.",
                        )
                    reservation = self._reserve_publish_locked()
                    self._active_operations += 1
                    operation_active = True
                published = self._publish_operation(
                    source, media_type=media_type, reservation=reservation
                )
                with self._lock:
                    reference, record = self._finish_published_locked(
                        published, expires_at=expires_at
                    )
                    if self._closing or self._closed:
                        self._unlink_record_best_effort(record)
                        raise ArtifactError(
                            "ARTIFACT.STORE_CLOSED",
                            "Artifact execution scope is closed.",
                        )
                    self._commit_publish_locked(reservation, reference, record)
                    return reference
            finally:
                with self._lock:
                    if reservation is not None and reservation.active:
                        self._release_publish_reservation_locked(reservation)
                    if operation_active:
                        self._active_operations -= 1
                        self._condition.notify_all()

    def resolve(
        self, reference: ArtifactRef | Mapping[str, Any]
    ) -> ArtifactHandle:
        ref = _coerce_ref(reference)
        reservation: _ResolveReservation | None = None
        operation_active = False
        try:
            with self._lock:
                self._ensure_open()
                self._validate_root_fd()
                record = self._records.get(ref.artifact_id)
                if record is None:
                    raise ArtifactError(
                        "ARTIFACT.SCOPE_MISMATCH",
                        "Artifact reference does not belong to this execution scope.",
                    )
                if not _refs_equal(ref, record.ref):
                    raise ArtifactError(
                        "ARTIFACT.REF_MISMATCH",
                        "Artifact reference metadata does not match the Host record.",
                    )
                now = _read_clock(self._clock, initial=False)
                if now >= record.expires_at:
                    self._expire_record_locked(ref.artifact_id)
                    raise ArtifactError(
                        "ARTIFACT.EXPIRED",
                        "Artifact reference has expired.",
                    )
                reservation = self._reserve_resolve_locked(
                    record.ref.artifact_id, record.ref.size_bytes
                )
                self._active_operations += 1
                operation_active = True
            payload = self._read_record_operation(record)
            with self._lock:
                self._ensure_open()
                current = self._records.get(ref.artifact_id)
                if current is not record:
                    raise ArtifactError(
                        "ARTIFACT.SCOPE_MISMATCH",
                        "Artifact reference is no longer available.",
                    )
                if _read_clock(self._clock, initial=False) >= record.expires_at:
                    self._expire_record_locked(ref.artifact_id)
                    raise ArtifactError(
                        "ARTIFACT.EXPIRED",
                        "Artifact reference has expired.",
                    )
                handle = ArtifactHandle(
                    payload,
                    record.ref,
                    expires_at=record.expires_at,
                    clock=self._clock,
                    on_close=self._release_handle,
                    on_expire=self._handle_expired,
                )
                self._commit_resolve_locked(reservation, handle)
                return handle
        finally:
            with self._lock:
                if reservation is not None and reservation.active:
                    self._release_resolve_reservation_locked(reservation)
                if operation_active:
                    self._active_operations -= 1
                    self._condition.notify_all()

    def _stage_batch_source(
        self,
        batch: ArtifactBatch,
        source: BinaryIO,
        *,
        media_type: str | None,
        ttl_seconds: float | None,
    ) -> ArtifactRef:
        if batch._store is not self or batch._closed:
            raise ArtifactError(
                "ARTIFACT.BATCH_CLOSED", "Artifact publication batch is closed."
            )
        if not callable(getattr(source, "read", None)):
            raise ArtifactError(
                "ARTIFACT.INVALID_SOURCE",
                "Artifact source must be a binary reader.",
            )
        ttl = self._ttl_seconds if ttl_seconds is None else _positive_number(
            ttl_seconds, "ttl_seconds"
        )
        reservation: _PublishReservation | None = None
        record: _ArtifactRecord | None = None
        staged = False
        operation_active = False
        with self._publish_lock:
            try:
                with self._lock:
                    self._ensure_open()
                    self._validate_root_fd()
                    now = _read_clock(self._clock, initial=False)
                    expires_at = now + ttl
                    if not math.isfinite(expires_at):
                        raise ArtifactError(
                            "ARTIFACT.TTL_OVERFLOW",
                            "Artifact expiry exceeds the supported clock range.",
                        )
                    reservation = self._reserve_publish_locked()
                    self._active_operations += 1
                    operation_active = True
                published = self._publish_operation(
                    source, media_type=media_type, reservation=reservation
                )
                with self._lock:
                    reference, record = self._finish_published_locked(
                        published, expires_at=expires_at
                    )
                    if self._closing or self._closed:
                        self._unlink_record_best_effort(record)
                        raise ArtifactError(
                            "ARTIFACT.STORE_CLOSED",
                            "Artifact execution scope is closed.",
                        )
                    self._provisional_ids.add(reference.artifact_id)
                    batch._entries.append((reference, record, reservation))
                    self._active_batches.add(batch)
                    self._temporary_names.discard(record.storage_name)
                    staged = True
                    return reference
            except BaseException:
                if record is not None:
                    with self._lock:
                        self._unlink_record_best_effort(record)
                raise
            finally:
                with self._lock:
                    if reservation is not None and reservation.active and not staged:
                        self._release_publish_reservation_locked(reservation)
                    if operation_active:
                        self._active_operations -= 1
                        self._condition.notify_all()

    def _commit_batch(self, batch: ArtifactBatch) -> tuple[ArtifactRef, ...]:
        if batch._store is not self or batch._closed:
            raise ArtifactError(
                "ARTIFACT.BATCH_CLOSED", "Artifact publication batch is closed."
            )
        with self._lock:
            self._ensure_open()
            self._validate_root_fd()
            entries = tuple(batch._entries)
            references = tuple(reference for reference, _, _ in entries)
            if not entries:
                raise ArtifactError(
                    "ARTIFACT.EMPTY_BATCH",
                    "Artifact publication batch has no staged artifacts.",
                )
            if len(self._records) + len(entries) > self._max_artifacts:
                _quota_error("maxArtifacts", self._max_artifacts)
            total = sum(record.ref.size_bytes for _, record, _ in entries)
            if self._stored_bytes + total > self._max_total_bytes:
                _quota_error("maxTotalBytes", self._max_total_bytes)
            if any(reference.artifact_id in self._records for reference, _, _ in entries):
                raise ArtifactError(
                    "ARTIFACT.INTERNAL", "Artifact identifier collision."
                )
            if self._memory_backend:
                replacement = dict(self._records)
                for reference, record, _reservation in entries:
                    replacement[reference.artifact_id] = record
                for reference, _record, _reservation in entries:
                    self._provisional_ids.discard(reference.artifact_id)
                self._records = replacement
                self._stored_bytes += total
                for _reference, _record, reservation in entries:
                    self._release_publish_reservation_locked(reservation)
                batch._entries.clear()
                self._active_batches.discard(batch)
                return references
            stored_bytes_before = self._stored_bytes
            committed: list[tuple[ArtifactRef, _ArtifactRecord]] = []
            try:
                for reference, record, reservation in entries:
                    committed.append((reference, record))
                    self._records[reference.artifact_id] = record
                    self._provisional_ids.discard(reference.artifact_id)
                    self._stored_bytes += record.ref.size_bytes
                for _reference, _record, reservation in entries:
                    self._release_publish_reservation_locked(reservation)
            except BaseException:
                for reference, record in committed:
                    if self._records.get(reference.artifact_id) is record:
                        self._records.pop(reference.artifact_id, None)
                        self._provisional_ids.add(reference.artifact_id)
                self._stored_bytes = stored_bytes_before
                raise
            batch._entries.clear()
            self._active_batches.discard(batch)
            return references

    def _rollback_batch(self, batch: ArtifactBatch) -> None:
        if batch._store is not self:
            return
        with self._lock:
            self._rollback_batch_locked(batch)

    def _rollback_batch_locked(self, batch: ArtifactBatch) -> None:
        for reference, record, reservation in batch._entries:
            self._unlink_record_best_effort(record)
            self._provisional_ids.discard(reference.artifact_id)
            if reservation.active:
                self._release_publish_reservation_noexcept_locked(reservation)
        batch._entries.clear()
        self._active_batches.discard(batch)

    def purge_expired(self) -> int:
        """Remove expired records/files and expire all issued handles."""

        with self._lock:
            self._ensure_open()
            self._validate_root_fd()
            now = _read_clock(self._clock, initial=False)
            expired_ids = [
                artifact_id
                for artifact_id, record in self._records.items()
                if now >= record.expires_at
            ]
            for artifact_id in expired_ids:
                self._expire_record_locked(artifact_id)
            return len(expired_ids)

    def cleanup(self) -> None:
        """Destroy this ephemeral scope; repeated calls are harmless."""

        with self._lock:
            if self._closing:
                while not self._closed:
                    self._condition.wait()
                return
            if self._closed:
                return
            self._closing = True
            while self._active_operations:
                self._condition.wait()
            for handle in tuple(self._handles):
                handle._close_from_store()
            self._handles.clear()
            for batch in tuple(self._active_batches):
                self._rollback_batch_locked(batch)
                batch._closed = True
            self._records.clear()
            self._open_handle_count = 0
            self._resolved_bytes = 0
            self._reserved_handles = 0
            self._reserved_resolved_bytes = 0
            self._stored_bytes = 0
            self._reserved_artifacts = 0
            self._reserved_storage_bytes = 0
            self._temporary_names.clear()
            self._provisional_ids.clear()
        try:
            with self._lock:
                if not self._memory_backend:
                    self._validate_root_fd()
                    self._clear_root_locked()
            if not self._memory_backend:
                _fsync_directory_fd(self._root_fd)
        except ArtifactError:
            pass
        finally:
            _close_fd(self._root_fd)
            _close_fd(self._parent_fd)
            with self._lock:
                self._root_fd = -1
                self._parent_fd = -1
                self._closed = True
                self._closing = False
                self._condition.notify_all()

    close = cleanup

    def _publish_operation(
        self,
        source: BinaryIO,
        *,
        media_type: str | None,
        reservation: _PublishReservation,
    ) -> _PublishedArtifact:
        if self._memory_backend:
            return self._publish_memory_operation(
                source, media_type=media_type, reservation=reservation
            )
        with self._lock:
            self._ensure_open()
            staging = self._new_storage_name_locked()
            destination = self._new_storage_name_locked(exclude={staging})
            self._temporary_names.update((staging, destination))
        source_fd = -1
        destination_fd = -1
        staging_info: os.stat_result | None = None
        destination_info: os.stat_result | None = None
        keep_destination = False
        try:
            flags = (
                os.O_RDWR
                | os.O_CREAT
                | os.O_EXCL
                | getattr(os, "O_CLOEXEC", 0)
                | os.O_NOFOLLOW
            )
            source_fd = os.open(
                staging, flags, stat.S_IRUSR | stat.S_IWUSR, dir_fd=self._root_fd
            )
            digest = hashlib.sha256()
            size = 0
            while True:
                try:
                    chunk = source.read(_CHUNK_SIZE)
                except Exception:
                    raise ArtifactError(
                        "ARTIFACT.SOURCE_READ_FAILED",
                        "Artifact source could not be read.",
                    ) from None
                if chunk == b"":
                    break
                if type(chunk) not in (bytes, bytearray, memoryview):
                    raise ArtifactError(
                        "ARTIFACT.INVALID_SOURCE",
                        "Artifact source did not return binary data.",
                    )
                try:
                    chunk_size = memoryview(chunk).nbytes
                except (TypeError, ValueError):
                    raise ArtifactError(
                        "ARTIFACT.INVALID_SOURCE",
                        "Artifact source did not return contiguous binary data.",
                    ) from None
                if chunk_size > _CHUNK_SIZE:
                    raise ArtifactError(
                        "ARTIFACT.INVALID_SOURCE",
                        "Artifact source returned more bytes than requested.",
                    )
                if size + chunk_size > self._max_size_bytes:
                    raise ArtifactError(
                        "ARTIFACT.SIZE_LIMIT_EXCEEDED",
                        "Artifact exceeds the configured byte limit.",
                        details={"maxSizeBytes": self._max_size_bytes},
                    )
                block = bytes(chunk)
                if len(block) != chunk_size:
                    raise ArtifactError(
                        "ARTIFACT.INVALID_SOURCE",
                        "Artifact source returned inconsistent binary data.",
                    )
                size += len(block)
                self._grow_publish_reservation(reservation, len(block))
                digest.update(block)
                _write_all(source_fd, block)
            os.fsync(source_fd)
            os.lseek(source_fd, 0, os.SEEK_SET)
            detected_media = self._validate_image(source_fd, media_type)
            os.fchmod(source_fd, _PUBLISHED_MODE)
            os.fsync(source_fd)
            staging_info = os.fstat(source_fd)
            _validate_staging_stat(staging_info, self._root_owner, size)
            _validate_leaf_namespace(self._root_fd, staging, staging_info)
            os.link(
                staging,
                destination,
                src_dir_fd=self._root_fd,
                dst_dir_fd=self._root_fd,
                follow_symlinks=False,
            )
            destination_info = staging_info
            destination_fd = os.open(destination, _READ_FLAGS, dir_fd=self._root_fd)
            linked_info = os.fstat(destination_fd)
            if not _same_inode(staging_info, linked_info):
                _integrity_error()
            destination_info = linked_info
            os.unlink(staging, dir_fd=self._root_fd)
            self._temporary_names.discard(staging)
            final_destination = os.fstat(destination_fd)
            _validate_published_stat(final_destination, self._root_owner, size)
            _validate_leaf_namespace(
                self._root_fd, destination, final_destination
            )
            _fsync_directory_fd(self._root_fd)
            published = _PublishedArtifact(
                storage_name=destination,
                digest="sha256:" + digest.hexdigest(),
                media_type=detected_media,
                size_bytes=size,
                info=final_destination,
            )
            keep_destination = True
            return published
        except ArtifactError:
            raise
        except (OSError, ValueError):
            raise ArtifactError(
                "ARTIFACT.PUBLISH_FAILED",
                "Artifact could not be published securely.",
            ) from None
        finally:
            if source_fd >= 0 and staging_info is None:
                try:
                    staging_info = os.fstat(source_fd)
                except OSError:
                    pass
            _close_fd(destination_fd)
            _close_fd(source_fd)
            if staging_info is not None:
                self._unlink_expected_leaf_best_effort_locked(staging, staging_info)
            self._temporary_names.discard(staging)
            if not keep_destination and destination_info is not None:
                self._unlink_expected_leaf_best_effort_locked(
                    destination, destination_info
                )
                self._temporary_names.discard(destination)

    def _read_record_operation(self, record: _ArtifactRecord) -> bytes:
        if self._memory_backend:
            payload = record.payload
            if type(payload) is not bytes:
                _integrity_error()
            actual_digest = "sha256:" + hashlib.sha256(payload).hexdigest()
            if len(payload) != record.ref.size_bytes or not hmac.compare_digest(
                actual_digest, record.ref.digest
            ):
                _integrity_error()
            return bytes(payload)
        descriptor = -1
        try:
            descriptor = os.open(
                record.storage_name, _READ_FLAGS, dir_fd=self._root_fd
            )
            before = os.fstat(descriptor)
            _validate_record_stat(before, record)
            payload = bytearray()
            digest = hashlib.sha256()
            while True:
                chunk = os.read(descriptor, min(_CHUNK_SIZE, self._max_size_bytes + 1 - len(payload)))
                if not chunk:
                    break
                payload.extend(chunk)
                digest.update(chunk)
                if len(payload) > self._max_size_bytes:
                    _integrity_error()
            after = os.fstat(descriptor)
            _validate_record_stat(after, record)
            _validate_leaf_namespace(self._root_fd, record.storage_name, after)
            actual_digest = "sha256:" + digest.hexdigest()
            if len(payload) != record.ref.size_bytes or not hmac.compare_digest(
                actual_digest, record.ref.digest
            ):
                _integrity_error()
            return bytes(payload)
        except ArtifactError:
            raise
        except (OSError, ValueError):
            _integrity_error()
        finally:
            _close_fd(descriptor)

    def _reserve_publish_locked(self) -> _PublishReservation:
        if self._reserved_artifacts + len(self._records) >= self._max_artifacts:
            _quota_error("maxArtifacts", self._max_artifacts)
        reservation = _PublishReservation(size_bytes=0)
        self._reserved_artifacts += 1
        return reservation

    def _grow_publish_reservation(
        self, reservation: _PublishReservation, amount: int
    ) -> None:
        with self._lock:
            self._ensure_open()
            if not reservation.active:
                raise ArtifactError(
                    "ARTIFACT.INTERNAL",
                    "Artifact reservation is no longer active.",
                )
            if (
                self._stored_bytes
                + self._reserved_storage_bytes
                + amount
                > self._max_total_bytes
            ):
                _quota_error("maxTotalBytes", self._max_total_bytes)
            reservation.size_bytes += amount
            self._reserved_storage_bytes += amount

    def _commit_publish_locked(
        self,
        reservation: _PublishReservation,
        reference: ArtifactRef,
        record: _ArtifactRecord,
    ) -> None:
        if not reservation.active:
            raise ArtifactError(
                "ARTIFACT.INTERNAL", "Artifact reservation is no longer active."
            )
        actual_size = record.ref.size_bytes
        if (
            len(self._records) >= self._max_artifacts
            or self._stored_bytes + actual_size > self._max_total_bytes
        ):
            self._unlink_record_best_effort(record)
            _quota_error("maxTotalBytes", self._max_total_bytes)
        self._records[reference.artifact_id] = record
        self._stored_bytes += actual_size
        self._release_publish_reservation_locked(reservation)
        self._temporary_names.discard(record.storage_name)

    def _release_publish_reservation_locked(
        self, reservation: _PublishReservation
    ) -> None:
        if not reservation.active:
            return
        reservation.active = False
        self._reserved_artifacts -= 1
        self._reserved_storage_bytes -= reservation.size_bytes

    def _release_publish_reservation_noexcept_locked(
        self, reservation: _PublishReservation
    ) -> None:
        """Release accounting during rollback without calling patchable hooks."""

        if not reservation.active:
            return
        reservation.active = False
        self._reserved_artifacts -= 1
        self._reserved_storage_bytes -= reservation.size_bytes

    def _finish_published_locked(
        self, published: _PublishedArtifact, *, expires_at: float
    ) -> tuple[ArtifactRef, _ArtifactRecord]:
        artifact_id = self._new_artifact_id_locked()
        reference = ArtifactRef(
            artifact_id=artifact_id,
            digest=published.digest,
            media_type=published.media_type,
            size_bytes=published.size_bytes,
        )
        info = published.info
        if self._memory_backend:
            if info is not None or type(published.payload) is not bytes:
                _integrity_error()
            return reference, _ArtifactRecord(
                ref=_copy_ref(reference),
                storage_name="",
                expires_at=expires_at,
                device=0,
                inode=0,
                owner=0,
                mode=0,
                links=0,
                modified_ns=0,
                changed_ns=0,
                payload=published.payload,
            )
        if info is None:
            _integrity_error()
        record = _ArtifactRecord(
            ref=_copy_ref(reference),
            storage_name=published.storage_name,
            expires_at=expires_at,
            device=info.st_dev,
            inode=info.st_ino,
            owner=info.st_uid,
            mode=stat.S_IMODE(info.st_mode),
            links=info.st_nlink,
            modified_ns=info.st_mtime_ns,
            changed_ns=info.st_ctime_ns,
        )
        return reference, record

    def _reserve_resolve_locked(
        self, artifact_id: str, size_bytes: int
    ) -> _ResolveReservation:
        if self._open_handle_count + self._reserved_handles >= self._max_open_handles:
            _quota_error("maxOpenHandles", self._max_open_handles)
        if (
            self._resolved_bytes
            + self._reserved_resolved_bytes
            + size_bytes
            > self._max_resolved_bytes
        ):
            _quota_error("maxResolvedBytes", self._max_resolved_bytes)
        reservation = _ResolveReservation(
            artifact_id=artifact_id, size_bytes=size_bytes
        )
        self._reserved_handles += 1
        self._reserved_resolved_bytes += size_bytes
        return reservation

    def _commit_resolve_locked(
        self, reservation: _ResolveReservation, handle: ArtifactHandle
    ) -> None:
        if not reservation.active:
            raise ArtifactError(
                "ARTIFACT.INTERNAL", "Artifact handle reservation is inactive."
            )
        self._open_handle_count += 1
        self._resolved_bytes += reservation.size_bytes
        self._handles.add(handle)
        self._release_resolve_reservation_locked(reservation)

    def _release_resolve_reservation_locked(
        self, reservation: _ResolveReservation
    ) -> None:
        if not reservation.active:
            return
        reservation.active = False
        self._reserved_handles -= 1
        self._reserved_resolved_bytes -= reservation.size_bytes

    def _discard_locked(self, artifact_id: str) -> None:
        record = self._records.pop(artifact_id, None)
        if record is not None:
            self._stored_bytes -= record.ref.size_bytes
            self._unlink_record_best_effort(record)

    def _expire_record_locked(self, artifact_id: str) -> None:
        self._discard_locked(artifact_id)
        for handle in tuple(self._handles):
            if handle._artifact_id == artifact_id:
                handle._expire_from_store()
                self._handles.discard(handle)
                self._open_handle_count -= 1
                self._resolved_bytes -= handle._size_bytes

    def _release_handle(self, handle: ArtifactHandle) -> None:
        with self._lock:
            if handle in self._handles:
                self._handles.discard(handle)
                self._open_handle_count -= 1
                self._resolved_bytes -= handle._size_bytes

    def _handle_expired(self, handle: ArtifactHandle) -> None:
        with self._lock:
            if handle in self._handles:
                self._handles.discard(handle)
                self._open_handle_count -= 1
                self._resolved_bytes -= handle._size_bytes
            if not self._closed:
                self._expire_record_locked(handle._artifact_id)

    def _unlink_record_best_effort(self, record: _ArtifactRecord) -> None:
        if self._memory_backend:
            return
        descriptor = -1
        try:
            descriptor = os.open(
                record.storage_name, _READ_FLAGS, dir_fd=self._root_fd
            )
            info = os.fstat(descriptor)
            if (
                stat.S_ISREG(info.st_mode)
                and (info.st_dev, info.st_ino) == (record.device, record.inode)
                and info.st_uid == record.owner
            ):
                _validate_leaf_namespace(self._root_fd, record.storage_name, info)
                os.unlink(record.storage_name, dir_fd=self._root_fd)
        except (ArtifactError, OSError):
            pass
        finally:
            _close_fd(descriptor)

    def _unlink_leaf_best_effort_locked(self, name: str) -> None:
        if _STORAGE_NAME.fullmatch(name) is None or self._root_fd < 0:
            return
        descriptor = -1
        try:
            descriptor = os.open(name, _READ_FLAGS, dir_fd=self._root_fd)
            info = os.fstat(descriptor)
            if stat.S_ISREG(info.st_mode) and info.st_uid == self._root_owner:
                _validate_leaf_namespace(self._root_fd, name, info)
                os.unlink(name, dir_fd=self._root_fd)
        except (ArtifactError, OSError):
            pass
        finally:
            _close_fd(descriptor)

    def _unlink_expected_leaf_best_effort_locked(
        self, name: str, expected: os.stat_result
    ) -> None:
        if _STORAGE_NAME.fullmatch(name) is None or self._root_fd < 0:
            return
        descriptor = -1
        try:
            descriptor = os.open(name, _READ_FLAGS, dir_fd=self._root_fd)
            info = os.fstat(descriptor)
            if (
                stat.S_ISREG(info.st_mode)
                and info.st_uid == self._root_owner
                and _same_inode(info, expected)
            ):
                _validate_leaf_namespace(self._root_fd, name, info)
                os.unlink(name, dir_fd=self._root_fd)
        except (ArtifactError, OSError):
            pass
        finally:
            _close_fd(descriptor)

    def _clear_root_locked(self) -> None:
        try:
            names = os.listdir(self._root_fd)
        except OSError:
            return
        for name in names:
            if type(name) is str:
                self._unlink_leaf_best_effort_locked(name)
        self._temporary_names.clear()

    def _new_storage_name_locked(self, *, exclude: set[str] | None = None) -> str:
        excluded = exclude or set()
        for _ in range(16):
            name = secrets.token_hex(32)
            if (
                _STORAGE_NAME.fullmatch(name) is not None
                and name not in excluded
                and name not in self._temporary_names
            ):
                return name
        raise ArtifactError(
            "ARTIFACT.PUBLISH_FAILED",
            "Artifact storage name generation failed.",
        )

    def _new_artifact_id_locked(self) -> str:
        for _ in range(16):
            artifact_id = "art_" + secrets.token_urlsafe(24)
            if (
                _ARTIFACT_ID.fullmatch(artifact_id)
                and artifact_id not in self._records
                and artifact_id not in self._provisional_ids
            ):
                return artifact_id
        raise ArtifactError(
            "ARTIFACT.PUBLISH_FAILED",
            "Artifact identifier generation failed.",
        )

    def _validate_root_fd(self) -> os.stat_result:
        if self._memory_backend:
            return os.stat_result((0,) * 10)
        if self._root_fd < 0:
            _integrity_error()
        try:
            info = os.fstat(self._root_fd)
        except OSError:
            _integrity_error()
        if (info.st_dev, info.st_ino) != (self._root_device, self._root_inode):
            _integrity_error()
        _validate_root_stat(info)
        return info

    def _publish_memory_operation(
        self,
        source: BinaryIO,
        *,
        media_type: str | None,
        reservation: _PublishReservation,
    ) -> _PublishedArtifact:
        payload = bytearray()
        digest = hashlib.sha256()
        while True:
            try:
                chunk = source.read(_CHUNK_SIZE)
            except Exception:
                raise ArtifactError(
                    "ARTIFACT.SOURCE_READ_FAILED",
                    "Artifact source could not be read.",
                ) from None
            if chunk == b"":
                break
            if type(chunk) not in (bytes, bytearray, memoryview):
                raise ArtifactError(
                    "ARTIFACT.INVALID_SOURCE",
                    "Artifact source did not return binary data.",
                )
            try:
                chunk_size = memoryview(chunk).nbytes
                block = bytes(chunk)
            except (TypeError, ValueError):
                raise ArtifactError(
                    "ARTIFACT.INVALID_SOURCE",
                    "Artifact source did not return contiguous binary data.",
                ) from None
            if chunk_size > _CHUNK_SIZE or len(block) != chunk_size:
                raise ArtifactError(
                    "ARTIFACT.INVALID_SOURCE",
                    "Artifact source returned an invalid byte block.",
                )
            if len(payload) + chunk_size > self._max_size_bytes:
                raise ArtifactError(
                    "ARTIFACT.SIZE_LIMIT_EXCEEDED",
                    "Artifact exceeds the configured byte limit.",
                    details={"maxSizeBytes": self._max_size_bytes},
                )
            self._grow_publish_reservation(reservation, chunk_size)
            payload.extend(block)
            digest.update(block)
        immutable = bytes(payload)
        detected_media = self._validate_image_bytes(immutable, media_type)
        return _PublishedArtifact(
            storage_name="",
            digest="sha256:" + digest.hexdigest(),
            media_type=detected_media,
            size_bytes=len(immutable),
            info=None,
            payload=immutable,
        )

    def _ensure_open(self) -> None:
        if self._closed or self._closing:
            raise ArtifactError(
                "ARTIFACT.STORE_CLOSED",
                "Artifact execution scope is closed.",
            )

    def _validate_image(self, descriptor: int, requested_media: str | None) -> str:
        if Image is None:
            raise ArtifactError(
                "ARTIFACT.DEPENDENCY_UNAVAILABLE",
                "Pillow is required to validate image artifacts.",
            )
        try:
            os.lseek(descriptor, 0, os.SEEK_SET)
            header = os.read(descriptor, 16)
            os.lseek(descriptor, 0, os.SEEK_SET)
        except OSError:
            raise ArtifactError(
                "ARTIFACT.INVALID_IMAGE",
                "Artifact image could not be inspected.",
            ) from None
        detected = _detect_media_type(header)
        if detected is None:
            raise ArtifactError(
                "ARTIFACT.UNSUPPORTED_MEDIA_TYPE",
                "Artifact is not a supported image format.",
            )
        if requested_media is not None:
            if type(requested_media) is not str or requested_media not in SUPPORTED_MEDIA_TYPES:
                raise ArtifactError(
                    "ARTIFACT.UNSUPPORTED_MEDIA_TYPE",
                    "Requested artifact media type is not supported.",
                )
            if not hmac.compare_digest(requested_media, detected):
                raise ArtifactError(
                    "ARTIFACT.MEDIA_TYPE_MISMATCH",
                    "Artifact bytes do not match the requested media type.",
                )
        expected_format = {
            "image/png": "PNG",
            "image/jpeg": "JPEG",
            "image/gif": "GIF",
            "image/tiff": "TIFF",
            "image/bmp": "BMP",
            "image/webp": "WEBP",
            "image/x-portable-anymap": "PPM",
        }[detected]
        try:
            with warnings.catch_warnings():
                warnings.simplefilter("error")
                with os.fdopen(os.dup(descriptor), "rb") as reader:
                    with Image.open(reader) as image:
                        self._validate_image_metadata(image, expected_format)
                        image.verify()
                os.lseek(descriptor, 0, os.SEEK_SET)
                with os.fdopen(os.dup(descriptor), "rb") as reader:
                    with Image.open(reader) as image:
                        self._validate_image_metadata(image, expected_format)
                        image.load()
        except ArtifactError:
            raise
        except (Image.DecompressionBombError, Image.DecompressionBombWarning):
            raise ArtifactError(
                "ARTIFACT.PIXEL_LIMIT_EXCEEDED",
                "Artifact image exceeds the decoder pixel safety limit.",
                details={"maxPixels": self._max_pixels},
            ) from None
        except Exception:
            raise ArtifactError(
                "ARTIFACT.INVALID_IMAGE",
                "Artifact image failed structural validation.",
            ) from None
        finally:
            try:
                os.lseek(descriptor, 0, os.SEEK_SET)
            except OSError:
                pass
        return detected

    def _validate_image_bytes(
        self, payload: bytes, requested_media: str | None
    ) -> str:
        return self._validate_image_reader(io.BytesIO(payload), requested_media)

    def _validate_image_reader(
        self, reader: BinaryIO, requested_media: str | None
    ) -> str:
        if Image is None:
            raise ArtifactError(
                "ARTIFACT.DEPENDENCY_UNAVAILABLE",
                "Pillow is required to validate image artifacts.",
            )
        try:
            reader.seek(0)
            header = reader.read(16)
            reader.seek(0)
        except Exception:
            raise ArtifactError(
                "ARTIFACT.INVALID_IMAGE",
                "Artifact image could not be inspected.",
            ) from None
        detected = _detect_media_type(header)
        if detected is None:
            raise ArtifactError(
                "ARTIFACT.UNSUPPORTED_MEDIA_TYPE",
                "Artifact is not a supported image format.",
            )
        if requested_media is not None:
            if type(requested_media) is not str or requested_media not in SUPPORTED_MEDIA_TYPES:
                raise ArtifactError(
                    "ARTIFACT.UNSUPPORTED_MEDIA_TYPE",
                    "Requested artifact media type is not supported.",
                )
            if not hmac.compare_digest(requested_media, detected):
                raise ArtifactError(
                    "ARTIFACT.MEDIA_TYPE_MISMATCH",
                    "Artifact bytes do not match the requested media type.",
                )
        expected_format = {
            "image/png": "PNG",
            "image/jpeg": "JPEG",
            "image/gif": "GIF",
            "image/tiff": "TIFF",
            "image/bmp": "BMP",
            "image/webp": "WEBP",
            "image/x-portable-anymap": "PPM",
        }[detected]
        try:
            with warnings.catch_warnings():
                warnings.simplefilter("error")
                reader.seek(0)
                with Image.open(reader) as image:
                    self._validate_image_metadata(image, expected_format)
                    image.verify()
                reader.seek(0)
                with Image.open(reader) as image:
                    self._validate_image_metadata(image, expected_format)
                    image.load()
        except ArtifactError:
            raise
        except (Image.DecompressionBombError, Image.DecompressionBombWarning):
            raise ArtifactError(
                "ARTIFACT.PIXEL_LIMIT_EXCEEDED",
                "Artifact image exceeds the decoder pixel safety limit.",
                details={"maxPixels": self._max_pixels},
            ) from None
        except Exception:
            raise ArtifactError(
                "ARTIFACT.INVALID_IMAGE",
                "Artifact image failed structural validation.",
            ) from None
        finally:
            try:
                reader.seek(0)
            except Exception:
                pass
        return detected

    def _validate_image_metadata(self, image: Any, expected_format: str) -> None:
        if image.format != expected_format:
            raise ArtifactError(
                "ARTIFACT.MEDIA_TYPE_MISMATCH",
                "Artifact decoder format does not match its magic bytes.",
            )
        width, height = image.size
        if (
            type(width) is not int
            or type(height) is not int
            or width <= 0
            or height <= 0
        ):
            raise ArtifactError(
                "ARTIFACT.INVALID_IMAGE",
                "Artifact image dimensions are invalid.",
            )
        if width > self._max_dimension or height > self._max_dimension:
            raise ArtifactError(
                "ARTIFACT.DIMENSION_LIMIT_EXCEEDED",
                "Artifact image exceeds the configured dimension limit.",
                details={"maxDimension": self._max_dimension},
            )
        if width * height > self._max_pixels:
            raise ArtifactError(
                "ARTIFACT.PIXEL_LIMIT_EXCEEDED",
                "Artifact image exceeds the configured pixel limit.",
                details={"maxPixels": self._max_pixels},
            )
        if getattr(image, "n_frames", 1) != 1:
            raise ArtifactError(
                "ARTIFACT.MULTI_FRAME_UNSUPPORTED",
                "Artifact image must contain exactly one frame.",
            )
        try:
            image.seek(1)
        except EOFError:
            return
        raise ArtifactError(
            "ARTIFACT.MULTI_FRAME_UNSUPPORTED",
            "Artifact image must contain exactly one frame.",
        )


def _require_supported_platform() -> None:
    if os.name == "nt":
        return
    required_dir_fd = (os.open, os.stat, os.link, os.unlink, os.rmdir)
    if (
        os.name != "posix"
        or not getattr(os, "O_NOFOLLOW", 0)
        or not getattr(os, "O_DIRECTORY", 0)
        or any(function not in os.supports_dir_fd for function in required_dir_fd)
        or os.stat not in os.supports_follow_symlinks
        or os.link not in os.supports_follow_symlinks
    ):
        raise ArtifactError(
            "ARTIFACT.PLATFORM_UNSUPPORTED",
            "Artifact storage requires Windows or qualified POSIX dir-fd primitives.",
        )


def _validate_ref_values(
    api_version: object,
    kind: object,
    artifact_id: object,
    digest: object,
    media_type: object,
    size_bytes: object,
) -> None:
    if (
        type(api_version) is not str
        or api_version != ARTIFACT_API_VERSION
        or type(kind) is not str
        or kind != ARTIFACT_KIND
        or type(artifact_id) is not str
        or len(artifact_id) != 36
        or _ARTIFACT_ID.fullmatch(artifact_id) is None
        or type(digest) is not str
        or len(digest) != 71
        or _DIGEST.fullmatch(digest) is None
        or type(media_type) is not str
        or media_type not in SUPPORTED_MEDIA_TYPES
        or type(size_bytes) is not int
        or size_bytes < 1
    ):
        _invalid_ref()


def _coerce_ref(value: ArtifactRef | Mapping[str, Any]) -> ArtifactRef:
    if isinstance(value, ArtifactRef):
        _validate_ref_values(
            value.api_version,
            value.kind,
            value.artifact_id,
            value.digest,
            value.media_type,
            value.size_bytes,
        )
        return value
    if not isinstance(value, Mapping):
        _invalid_ref()
    return ArtifactRef.from_dict(value)


def _refs_equal(left: ArtifactRef, right: ArtifactRef) -> bool:
    return (
        type(left.api_version) is str
        and type(left.kind) is str
        and type(left.artifact_id) is str
        and type(left.digest) is str
        and type(left.media_type) is str
        and type(left.size_bytes) is int
        and hmac.compare_digest(left.api_version, right.api_version)
        and hmac.compare_digest(left.kind, right.kind)
        and hmac.compare_digest(left.artifact_id, right.artifact_id)
        and hmac.compare_digest(left.digest, right.digest)
        and hmac.compare_digest(left.media_type, right.media_type)
        and left.size_bytes == right.size_bytes
    )


def _detect_media_type(header: bytes) -> str | None:
    if header.startswith(b"\x89PNG\r\n\x1a\n"):
        return "image/png"
    if header.startswith(b"\xff\xd8\xff"):
        return "image/jpeg"
    if header.startswith((b"GIF87a", b"GIF89a")):
        return "image/gif"
    if header.startswith((b"II*\x00", b"MM\x00*", b"II+\x00", b"MM\x00+")):
        return "image/tiff"
    if header.startswith(b"BM"):
        return "image/bmp"
    if len(header) >= 12 and header.startswith(b"RIFF") and header[8:12] == b"WEBP":
        return "image/webp"
    if (
        len(header) >= 3
        and header[:2] in {b"P1", b"P2", b"P3", b"P4", b"P5", b"P6"}
        and header[2:3].isspace()
    ):
        return "image/x-portable-anymap"
    return None


def _validate_root_stat(info: os.stat_result) -> None:
    if (
        not stat.S_ISDIR(info.st_mode)
        or stat.S_IMODE(info.st_mode) != 0o700
        or info.st_uid != os.geteuid()
    ):
        _integrity_error()


def _validate_staging_stat(
    info: os.stat_result, root_owner: int, expected_size: int
) -> None:
    if (
        not stat.S_ISREG(info.st_mode)
        or info.st_nlink != 1
        or info.st_uid != root_owner
        or stat.S_IMODE(info.st_mode) != _PUBLISHED_MODE
        or info.st_size != expected_size
    ):
        _integrity_error()


def _validate_published_stat(
    info: os.stat_result, root_owner: int, expected_size: int
) -> None:
    _validate_staging_stat(info, root_owner, expected_size)


def _validate_record_stat(info: os.stat_result, record: _ArtifactRecord) -> None:
    if (
        not stat.S_ISREG(info.st_mode)
        or (info.st_dev, info.st_ino) != (record.device, record.inode)
        or info.st_uid != record.owner
        or stat.S_IMODE(info.st_mode) != record.mode
        or record.mode != _PUBLISHED_MODE
        or info.st_nlink != record.links
        or info.st_nlink != 1
        or info.st_size != record.ref.size_bytes
        or info.st_mtime_ns != record.modified_ns
        or info.st_ctime_ns != record.changed_ns
    ):
        _integrity_error()


def _validate_leaf_namespace(
    directory_fd: int, name: str, opened: os.stat_result
) -> None:
    if _STORAGE_NAME.fullmatch(name) is None:
        _integrity_error()
    try:
        namespace = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
    except OSError:
        _integrity_error()
    if not _same_inode(namespace, opened):
        _integrity_error()


def _same_inode(left: os.stat_result, right: os.stat_result) -> bool:
    return (left.st_dev, left.st_ino) == (right.st_dev, right.st_ino)


def _stable_source(before: os.stat_result, after: os.stat_result) -> bool:
    return (
        _same_inode(before, after)
        and stat.S_ISREG(after.st_mode)
        and before.st_size == after.st_size
        and before.st_mtime_ns == after.st_mtime_ns
        and before.st_ctime_ns == after.st_ctime_ns
    )


def _namespace_is_same(
    parent_fd: int, name: str, expected: os.stat_result
) -> bool:
    if parent_fd < 0 or not name or os.sep in name or name in {".", ".."}:
        return False
    try:
        actual = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
    except OSError:
        return False
    return stat.S_ISDIR(actual.st_mode) and _same_inode(actual, expected)


def _rmdir_if_same(parent_fd: int, name: str, expected: os.stat_result) -> None:
    if not _namespace_is_same(parent_fd, name, expected):
        return
    try:
        os.rmdir(name, dir_fd=parent_fd)
    except OSError:
        pass


def _write_all(descriptor: int, data: bytes) -> None:
    view = memoryview(data)
    while view:
        try:
            written = os.write(descriptor, view)
        except OSError:
            raise ArtifactError(
                "ARTIFACT.PUBLISH_FAILED",
                "Artifact could not be published securely.",
            ) from None
        if written <= 0:
            raise ArtifactError(
                "ARTIFACT.PUBLISH_FAILED",
                "Artifact could not be published securely.",
            )
        view = view[written:]


def _fsync_directory_fd(descriptor: int) -> None:
    try:
        os.fsync(descriptor)
    except OSError as error:
        unsupported = {errno.EINVAL, errno.EBADF}
        if hasattr(errno, "ENOTSUP"):
            unsupported.add(errno.ENOTSUP)
        if hasattr(errno, "EOPNOTSUPP"):
            unsupported.add(errno.EOPNOTSUPP)
        if error.errno in unsupported:
            return
        raise ArtifactError(
            "ARTIFACT.PUBLISH_FAILED",
            "Artifact directory could not be synchronized.",
        ) from None


def _read_clock(clock: Callable[[], float], *, initial: bool) -> float:
    try:
        value = clock()
    except Exception:
        value = None
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or not math.isfinite(value)
        or value < 0
    ):
        raise ArtifactError(
            "ARTIFACT.INVALID_CONFIGURATION" if initial else "ARTIFACT.CLOCK_INVALID",
            "Artifact clock must return a finite nonnegative number.",
        )
    return float(value)


def _positive_number(value: object, name: str) -> float:
    if (
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or not math.isfinite(value)
        or value <= 0
    ):
        raise ArtifactError(
            "ARTIFACT.INVALID_CONFIGURATION",
            f"{name} must be a finite positive number.",
        )
    return float(value)


def _positive_integer(value: object, name: str) -> int:
    if type(value) is not int or value <= 0:
        raise ArtifactError(
            "ARTIFACT.INVALID_CONFIGURATION",
            f"{name} must be a positive integer.",
        )
    return value


def _close_fd(descriptor: int) -> None:
    if descriptor < 0:
        return
    try:
        os.close(descriptor)
    except OSError:
        pass


def _invalid_ref() -> None:
    raise ArtifactError(
        "ARTIFACT.INVALID_REF",
        "Artifact reference does not match the closed v1alpha1 contract.",
    )


def _integrity_error() -> None:
    raise ArtifactError(
        "ARTIFACT.INTEGRITY_FAILED",
        "Artifact storage integrity validation failed.",
    )


def _quota_error(limit: str, value: int) -> None:
    raise ArtifactError(
        "ARTIFACT.QUOTA_EXCEEDED",
        "Artifact store quota is exhausted.",
        details={"limit": limit, "value": value},
    )


__all__ = [
    "ARTIFACT_API_VERSION",
    "ARTIFACT_KIND",
    "ArtifactError",
    "ArtifactHandle",
    "ArtifactRef",
    "ArtifactStore",
    "DEFAULT_ARTIFACT_TTL_SECONDS",
    "DEFAULT_MAX_DIMENSION",
    "DEFAULT_MAX_ARTIFACTS",
    "DEFAULT_MAX_OPEN_HANDLES",
    "DEFAULT_MAX_PIXELS",
    "DEFAULT_MAX_RESOLVED_BYTES",
    "DEFAULT_MAX_SIZE_BYTES",
    "DEFAULT_MAX_TOTAL_BYTES",
    "SUPPORTED_MEDIA_TYPES",
]
