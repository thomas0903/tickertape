"""Type stubs for the compiled extension ``tickertape._native`` (re-exported by ``tickertape``)."""

import datetime
import os
from collections.abc import Iterable, Iterator
from pathlib import Path
from typing import Any, Final, Literal, Self, SupportsIndex, TypeAlias, TypedDict, final

from typing_extensions import Buffer  # collections.abc.Buffer from Python 3.12; stubs are never imported at runtime

from ._errors import TickertapeError
from ._inspect import CheckpointProgress, RecoveryReport, RetentionReport
from ._status import JournalStatus, ReaderMetrics, WriterMetrics


_Path: TypeAlias = str | os.PathLike[str]
_Seconds: TypeAlias = float | datetime.timedelta
# Any buffer of unsigned bytes (item format "B"): bytes, bytearray, memoryview, array("B"), uint8 NumPy arrays. Buffers
# of other item types are refused at runtime with TypeError.
_Bytes: TypeAlias = Buffer

__all__ = [
    "CORE_VERSION",
    "CORRUPT_REASONS",
    "DEFAULT_SEGMENT_SIZE",
    "ERROR_VARIANTS",
    "FORMAT_VERSION",
    "RECOVERY_ACTIONS",
    "SEGMENT_ENDS",
    "STATUS_ENUMS",
    "Backoff",
    "Reader",
    "Record",
    "RecordBatch",
    "Retention",
    "StatusSample",
    "Writer",
    "__version__",
    "list_segments",
    "read_checkpoint",
    "sample_status",
    "verify",
]

__version__: Final[str]
CORE_VERSION: Final[str]
FORMAT_VERSION: Final[int]
DEFAULT_SEGMENT_SIZE: Final[int]
ERROR_VARIANTS: Final[list[tuple[str, str]]]
CORRUPT_REASONS: Final[list[tuple[str, str]]]
RECOVERY_ACTIONS: Final[list[tuple[str, str]]]
SEGMENT_ENDS: Final[list[tuple[str, str]]]
STATUS_ENUMS: Final[list[tuple[str, list[tuple[str, str]]]]]

class _VerifyDict(TypedDict):
    records: int
    first_seq: int | None
    last_seq: int | None
    segments: list[tuple[Path, tuple[Path, int, int, int, int] | None, int, int, int, str]]
    problems: list[tuple[Path, int, str, dict[str, Any], str]]

@final
class Backoff:
    """How a Reader waits between polls; all waiting happens with the GIL released. Sleeps are at most 3600 s."""

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
    def __hash__(self) -> int: ...

@final
class Retention:
    """A retention policy and action, run by the writer after open and after each successful checkpoint."""

    @staticmethod
    def delete(
        *, max_segments: int | None = None, max_bytes: int | None = None, max_age: _Seconds | None = None
    ) -> Retention: ...
    @staticmethod
    def move_to(
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
    def move_to_directory(self) -> Path | None: ...
    def __eq__(self, other: object, /) -> bool: ...
    def __hash__(self) -> int: ...

@final
class Writer:
    """The single writer of a journal directory."""

    def __new__(
        cls,
        path: _Path,
        *,
        segment_size: int = 134217728,  # DEFAULT_SEGMENT_SIZE
        recovery: Literal["quarantine", "fail"] = "quarantine",
        notify: bool = True,
        prefault: bool = True,
        early_writeback: bool = True,
        helper_cpus: Literal["process", "inherited"] | Iterable[SupportsIndex] = "process",
        retention: Retention | None = None,
    ) -> Writer: ...
    def append(self, payload: _Bytes, *, schema_id: int = 0, venue_ts_ns: int = 0, local_ts_ns: int = 0) -> int: ...
    def append_batch(
        self,
        payloads: Iterable[_Bytes],
        *,
        schema_id: int | Iterable[SupportsIndex] | None = None,
        venue_ts_ns: int | Iterable[SupportsIndex] | None = None,
        local_ts_ns: int | Iterable[SupportsIndex] | None = None,
    ) -> range: ...
    def checkpoint(self) -> None: ...
    def request_checkpoint(self) -> int: ...
    def checkpoint_progress(self) -> CheckpointProgress: ...
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
    def last_retention_error(self) -> TickertapeError | None: ...
    def metrics(self) -> WriterMetrics: ...
    def close(self) -> None: ...
    def __enter__(self) -> Self: ...
    def __exit__(self, exc_type: object = None, exc_value: object = None, traceback: object = None) -> None: ...

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
    def __getitem__(self, index: SupportsIndex, /) -> Record:
        """Record ``index`` (negative counts from the end). Slicing and non-integer indices raise ``TypeError``; an
        integer out of range raises ``IndexError``."""
    def __iter__(self) -> Iterator[Record]: ...
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
    def seek_to_end(self) -> None:
        """Position the reader at the end of the journal, without reading the segments before it. Added in 1.1."""
    def try_next(self) -> Record | None: ...
    def next(self, timeout: _Seconds | None = None) -> Record | None:
        """Wait up to ``timeout`` seconds (default ``None``: forever) for the next record."""
    def read_many(self, max_records: int, *, timeout: _Seconds | None = 0) -> list[Record]:
        """Wait up to ``timeout`` seconds (default ``0``: don't wait; ``None``: forever) for the first record."""
    def read_batch(self, max_records: int, *, timeout: _Seconds | None = 0) -> RecordBatch:
        """Like ``read_many``, as a columnar batch."""
    @property
    def position(self) -> int: ...
    def metrics(self, status: JournalStatus) -> ReaderMetrics: ...
    @property
    def durable_only(self) -> bool: ...
    @property
    def path(self) -> Path: ...
    @property
    def closed(self) -> bool: ...
    def close(self) -> None: ...
    def __enter__(self) -> Self: ...
    def __exit__(self, *args: object) -> None: ...
    def __iter__(self) -> Iterator[Record]:
        """A new iterator over the records committed so far; it doesn't wait and stays exhausted at the end."""

def list_segments(path: _Path) -> list[tuple[Path, int, int, int, int]]: ...
def read_checkpoint(path: _Path) -> tuple[int, int, int, int] | None: ...
def verify(path: _Path) -> _VerifyDict: ...
@final
class StatusSample:
    def fields(self) -> dict[str, Any]: ...

def sample_status(path: _Path, readers_dir: _Path | None = None) -> StatusSample: ...
