"""Python bindings for tickertape, a local append-only event journal on memory-mapped files.

One :class:`Writer` appends records; any number of :class:`Reader` objects, in this or other processes on the same
host, replay the journal or follow it live. A record is *visible* to readers (and survives the writer process dying)
as soon as :meth:`Writer.append` returns, and *durable* only after a later :meth:`Writer.checkpoint` returns. See
https://github.com/thomas0903/tickertape/blob/main/docs/crash-contract.md.

Records returned to Python are copies: each payload is an ordinary ``bytes`` object that stays valid after the reader
moves on. The bindings use the core crate's safe ``pread`` reader; the zero-copy mapped reader is not exposed.

Handles belong to the process that opened them: in a child created by ``fork()`` they raise
:class:`UsedAfterForkError`. Open them after forking, or use the ``spawn`` start method of :mod:`multiprocessing`.

For read-only access without installing anything, the repository also has a dependency-free decoder,
``python/tickertape_reader.py``.
"""

from . import _native
from ._errors import (
    BatchAppendError,
    CorruptJournalError,
    DiscardedError,
    DurabilityLostError,
    InvalidConfigError,
    JournalIOError,
    JournalNotFoundError,
    LockedError,
    RecordTooLargeError,
    SeqUnavailableError,
    SequenceExhaustedError,
    StorageFullError,
    TickertapeError,
    UnsupportedVersionError,
    UsedAfterForkError,
)
from ._inspect import (
    Checkpoint,
    Problem,
    RecoveryAction,
    RecoveryReport,
    RetentionReport,
    SegmentInfo,
    SegmentReport,
    VerifyReport,
    list_segments,
    read_checkpoint,
    verify,
)
from ._native import (
    CORE_VERSION,
    DEFAULT_SEGMENT_SIZE,
    FORMAT_VERSION,
    Backoff,
    Reader,
    Record,
    RecordBatch,
    Retention,
    Writer,
    __version__,
)

CORRUPT_REASONS: tuple[str, ...] = tuple(code for _, code in _native.CORRUPT_REASONS) + ("unknown",)
"""Every value of :attr:`CorruptJournalError.reason` and :attr:`Problem.reason`. ``"unknown"`` appears only for a
reason added to the core library after these bindings were built."""

__all__ = [
    "CORE_VERSION",
    "CORRUPT_REASONS",
    "DEFAULT_SEGMENT_SIZE",
    "FORMAT_VERSION",
    "Backoff",
    "BatchAppendError",
    "Checkpoint",
    "CorruptJournalError",
    "DiscardedError",
    "DurabilityLostError",
    "InvalidConfigError",
    "JournalIOError",
    "JournalNotFoundError",
    "LockedError",
    "Problem",
    "Reader",
    "Record",
    "RecordBatch",
    "RecordTooLargeError",
    "RecoveryAction",
    "RecoveryReport",
    "Retention",
    "RetentionReport",
    "SegmentInfo",
    "SegmentReport",
    "SeqUnavailableError",
    "SequenceExhaustedError",
    "StorageFullError",
    "TickertapeError",
    "UnsupportedVersionError",
    "UsedAfterForkError",
    "VerifyReport",
    "Writer",
    "__version__",
    "list_segments",
    "read_checkpoint",
    "verify",
]
