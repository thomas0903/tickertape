"""Type stubs for the compiled extension ``tickertape._native`` (re-exported by ``tickertape``)."""

import datetime
import os
from collections.abc import Iterable, Sequence
from pathlib import Path
from typing import Any, Final, Literal, Self, SupportsIndex, TypeAlias, TypedDict, final, type_check_only


_Path: TypeAlias = str | os.PathLike[str]
_Seconds: TypeAlias = float | datetime.timedelta
_Bytes: TypeAlias = bytes | bytearray | memoryview

__all__ = [
    "CORE_VERSION",
    "CORRUPT_REASONS",
    "DEFAULT_SEGMENT_SIZE",
    "ERROR_VARIANTS",
    "FORMAT_VERSION",
    "Backoff",
    "Reader",
    "Record",
    "RecordBatch",
    "Retention",
    "Writer",
    "__version__",
    "list_segments",
    "read_checkpoint",
    "verify",
]

__version__: Final[str]
CORE_VERSION: Final[str]
FORMAT_VERSION: Final[int]
DEFAULT_SEGMENT_SIZE: Final[int]
ERROR_VARIANTS: Final[list[tuple[str, str]]]
CORRUPT_REASONS: Final[list[tuple[str, str]]]

@type_check_only
class RecoveryReport(TypedDict):
    next_seq: int
    scanned_from_seq: int
    had_checkpoint: bool
    actions: list[dict[str, Any]]

@type_check_only
class RetentionReport(TypedDict):
    removed_segments: int
    removed_bytes: int
    already_removed_segments: int

class _VerifyDict(TypedDict):
    records: int
    first_seq: int | None
    last_seq: int | None
    segments: list[tuple[Path, tuple[Path, int, int, int, int] | None, int, int, int, str]]
    problems: list[tuple[Path, int, str, dict[str, Any], str]]

@final
class Backoff:
    """How a Reader waits between polls; all waiting happens with the GIL released."""

    def __new__(
        cls,
        *,
        spins: int = 100,
        yields: int = 10,
        min_sleep: _Seconds | None = None,
        max_sleep: _Seconds | None = None,
    ) -> Backoff: ...
    @staticmethod
    def busy_spin() -> Backoff: ...
    @property
    def spins(self) -> int: ...
    @property
    def yields(self) -> int: ...
    @property
    def min_sleep(self) -> float: ...
    @property
    def max_sleep(self) -> float: ...
    def __eq__(self, other: object, /) -> bool: ...

@final
class Retention:
    """A retention policy and action, run by the writer after open and after each successful checkpoint."""

    @staticmethod
    def delete(
        *, max_segments: int | None = None, max_bytes: int | None = None, max_age: _Seconds | None = None
    ) -> Retention: ...
    @staticmethod
    def archive(
        directory: _Path,
        *,
        max_segments: int | None = None,
        max_bytes: int | None = None,
        max_age: _Seconds | None = None,
    ) -> Retention: ...
    @property
    def max_segments(self) -> int | None: ...
    @property
    def max_bytes(self) -> int | None: ...
    @property
    def max_age(self) -> float | None: ...
    @property
    def archive_dir(self) -> Path | None: ...

@final
class Writer:
    """The single writer of a journal directory."""

    def __new__(
        cls,
        path: _Path,
        *,
        segment_size: int = ...,
        recovery: Literal["quarantine", "fail"] = "quarantine",
        notify: bool = True,
        prefault: bool = True,
        retention: Retention | None = None,
    ) -> Writer: ...
    def append(self, payload: _Bytes, *, schema_id: int = 0, venue_ts_ns: int = 0, local_ts_ns: int = 0) -> int: ...
    def append_many(
        self,
        payloads: Iterable[_Bytes],
        *,
        schema_id: int | Sequence[SupportsIndex] | None = None,
        venue_ts_ns: int | Sequence[SupportsIndex] | None = None,
        local_ts_ns: int | Sequence[SupportsIndex] | None = None,
    ) -> range: ...
    def checkpoint(self) -> None: ...
    @property
    def next_seq(self) -> int: ...
    @property
    def path(self) -> Path: ...
    @property
    def closed(self) -> bool: ...
    @property
    def recovery_report(self) -> RecoveryReport: ...
    @property
    def retention_report(self) -> RetentionReport: ...
    @property
    def last_retention_error(self) -> BaseException | None: ...
    def close(self) -> None: ...
    def __enter__(self) -> Self: ...
    def __exit__(self, *args: object) -> None: ...

@final
class Record:
    """One record; owns its payload (a ``bytes`` copy)."""

    @property
    def seq(self) -> int: ...
    @property
    def schema_id(self) -> int: ...
    @property
    def venue_ts_ns(self) -> int: ...
    @property
    def local_ts_ns(self) -> int: ...
    @property
    def payload(self) -> bytes: ...
    def as_tuple(self) -> tuple[int, int, int, int, bytes]: ...
    def as_dict(self) -> dict[str, int | bytes]: ...
    def __eq__(self, other: object, /) -> bool: ...
    def __hash__(self) -> int: ...

@final
class RecordBatch:
    """Records as little-endian columns (``bytes``); see ``to_numpy``/``to_arrow``/``to_pandas``."""

    @property
    def seq(self) -> bytes: ...
    @property
    def schema_id(self) -> bytes: ...
    @property
    def venue_ts_ns(self) -> bytes: ...
    @property
    def local_ts_ns(self) -> bytes: ...
    @property
    def payload_offsets(self) -> bytes: ...
    @property
    def payload_data(self) -> bytes: ...
    def __len__(self) -> int: ...
    def __getitem__(self, index: int, /) -> Record: ...
    def records(self) -> list[Record]: ...
    def to_numpy(self) -> dict[str, Any]: ...
    def to_arrow(self) -> Any: ...
    def to_pandas(self) -> Any: ...

@final
class Reader:
    """An independent cursor over a journal (safe ``pread`` reader)."""

    def __new__(
        cls, path: _Path, *, durable_only: bool = False, notify: bool = True, backoff: Backoff | None = None
    ) -> Reader: ...
    def seek(self, seq: int) -> None: ...
    def try_next(self) -> Record | None: ...
    def next(self, timeout: _Seconds | None = ...) -> Record | None: ...
    def read_many(self, max_records: int, *, timeout: _Seconds | None = ...) -> list[Record]: ...
    def read_batch(self, max_records: int, *, timeout: _Seconds | None = ...) -> RecordBatch: ...
    @property
    def position(self) -> int: ...
    @property
    def durable_only(self) -> bool: ...
    @property
    def path(self) -> Path: ...
    @property
    def closed(self) -> bool: ...
    def close(self) -> None: ...
    def __enter__(self) -> Self: ...
    def __exit__(self, *args: object) -> None: ...
    def __iter__(self) -> Self: ...
    def __next__(self) -> Record: ...

def list_segments(path: _Path) -> list[tuple[Path, int, int, int, int]]: ...
def read_checkpoint(path: _Path) -> tuple[int, int, int, int] | None: ...
def verify(path: _Path) -> _VerifyDict: ...
