"""Exception hierarchy of the tickertape bindings.

Every exception raised for a ``tickertape::Error`` is a :class:`TickertapeError`, and so is
:class:`UsedAfterForkError`. Some also inherit from the built-in exception that matches their meaning, so generic
handlers work too: :class:`JournalIOError` is an :class:`OSError`, :class:`RecordTooLargeError` and
:class:`InvalidConfigError` are :class:`ValueError`, and :class:`SeqUnavailableError` is a :class:`LookupError`.

Every exception keeps its message and attributes, with their current values, across ``pickle`` (for example when it is
sent back from a ``multiprocessing`` worker). ``__cause__`` is not pickled, as for every Python exception.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Optional


def _rebuild(cls: type[TickertapeError], message: str, fields: dict[str, Any],
             without_message: bool = False) -> TickertapeError:
    error = cls(message, **fields)
    if without_message:
        del error.message
    return error


class TickertapeError(Exception):
    """Base class of every error raised for a ``tickertape::Error``, and of :class:`UsedAfterForkError`."""

    message: str

    def __init__(self, message: str, **fields: Any) -> None:
        super().__init__(message)
        self.message = message
        self._tickertape_fields = fields
        for name, value in fields.items():
            setattr(self, name, value)

    def __str__(self) -> str:
        return self.message

    def __reduce__(self) -> tuple[Any, ...]:
        # Rebuild from the current message and field values: OSError keeps errno, strerror and filename outside
        # __dict__, so the default reduction would lose them. Then restore anything else set on the instance (such as
        # __notes__). An attribute the caller deleted stays deleted in the copy.
        names = list(getattr(self, "_tickertape_fields", ()))
        if isinstance(self, OSError):
            names += [name for name in ("errno", "strerror", "filename") if name not in names]
        missing = object()
        fields = {name: value for name in names if (value := getattr(self, name, missing)) is not missing}
        message = getattr(self, "message", missing)
        if message is missing:
            # Cannot raise: nothing is called on the argument. Its type comes from type(), not isinstance() (which
            # reads a __class__ attribute that can raise), and a str is copied without calling a subclass's __str__.
            first = self.args[0] if self.args else None
            message = str.__str__(first) if issubclass(type(first), str) else "(message deleted)"
        state = {key: value for key, value in self.__dict__.items()
                 if key not in fields and key not in ("message", "_tickertape_fields")}
        return (_rebuild, (type(self), message, fields, "message" not in self.__dict__), state or None)


class JournalIOError(TickertapeError, OSError):
    """An operating-system call failed. ``errno``, ``strerror`` (the OS message, as ``os.strerror(errno)``) and
    ``filename`` (a :class:`~pathlib.Path`) are set as for :class:`OSError`."""

    op: str
    """What tickertape was doing, for example ``"preallocate segment"``."""
    storage_full: bool
    """True for ``ENOSPC``/``EDQUOT``. Such append failures leave the journal consistent and can be retried."""


class StorageFullError(JournalIOError):
    """The filesystem is full (``ENOSPC``, ``EDQUOT``). Appends can be retried once space is available."""


class LockedError(TickertapeError):
    """Another writer holds the journal's lock."""

    path: Path
    pid: Optional[int]
    """The PID recorded in ``writer.lock``, if it could be read. Informational only."""


class CorruptJournalError(TickertapeError):
    """Bytes in a segment failed validation. The reader's cursor does not move; reading again retries."""

    path: Path
    offset: int
    """Byte offset of the invalid header, record or seal."""
    reason: str
    """A stable code such as ``"bad_checksum"`` (see ``tickertape.CORRUPT_REASONS``)."""
    details: dict[str, Any]
    """The reason's fields, for example ``{"expected": 5, "found": 7}`` for ``"bad_sequence"``."""


class UnsupportedVersionError(TickertapeError):
    """A segment was written by an incompatible format version."""

    path: Path
    found: int
    supported: int


class JournalNotFoundError(TickertapeError):
    """The directory contains no segments (for example, the writer hasn't started yet)."""

    path: Path


class SeqUnavailableError(TickertapeError, LookupError):
    """A requested sequence number is older than the oldest segment in the journal."""

    requested: int
    first: int


class RecordTooLargeError(TickertapeError, ValueError):
    """A payload does not fit in one segment (or in the current, still empty segment)."""

    length: int
    max: int


class DurabilityLostError(TickertapeError):
    """An earlier checkpoint failed; this writer can no longer guarantee durability. Appends still work."""


class SequenceExhaustedError(TickertapeError):
    """The next sequence number would be 2**64 - 1."""


class DiscardedError(TickertapeError):
    """Recovery or retention replaced or removed the segment at the reader's position. Call ``seek`` to continue."""

    seq: int


class InvalidConfigError(TickertapeError, ValueError):
    """A configuration value is out of range."""


class UsedAfterForkError(TickertapeError):
    """A :class:`Writer` or :class:`Reader` was used in a child process created by ``fork()``.

    A handle belongs to the process that opened it (its process id and a count of forks, so a reused process id is
    still recognised). A forked child gets a copy that shares the parent's memory mappings, file descriptors and writer
    lock but not its helper threads, so using it could overwrite records or hang. Every method except ``close()``
    raises this in the child, before touching anything. ``close()``, ``with`` and garbage collection in the child
    release the child's copies of the descriptors and mappings, without unlocking, syncing, or changing any file, so
    the journal can be reopened once the parent closes its writer. A handle that another thread was using at the
    moment of the fork is never released in the child: its descriptors stay open until the child exits or calls
    ``exec``. Open handles after forking, or use the ``spawn`` or ``forkserver`` start method of
    :mod:`multiprocessing`.
    """

    owner_pid: int
    """The process that opened the handle."""
    pid: int
    """The process that tried to use it."""


class BatchAppendError(TickertapeError):
    """``Writer.append_many`` failed part-way.

    Records ``payloads[:appended]`` were appended (sequence numbers ``first_seq`` to ``first_seq + appended - 1``,
    visible but not yet durable); ``payloads[failed_index:]`` were not. The error that stopped the batch is
    ``__cause__``.

    To retry once the cause is resolved, call ``append_many`` again with ``payloads[failed_index:]`` and every
    per-record metadata sequence sliced the same way (``schema_id=schema_ids[failed_index:]``, ...); a single int
    metadata value needs no slicing. ``append_many`` consumes its arguments, so a caller who may retry must pass
    sequences it can slice (lists, tuples, arrays), not one-shot iterators or generators. Converting the arguments and
    checking the column lengths happens before anything is appended, but an append can still fail part-way, for
    example with :class:`RecordTooLargeError` for a payload that does not fit in a segment or
    :class:`StorageFullError`.
    """

    appended: int
    first_seq: Optional[int]
    """``None`` when nothing was appended."""
    failed_index: int


__all__ = [
    "BatchAppendError",
    "CorruptJournalError",
    "DiscardedError",
    "DurabilityLostError",
    "InvalidConfigError",
    "JournalIOError",
    "JournalNotFoundError",
    "LockedError",
    "RecordTooLargeError",
    "SeqUnavailableError",
    "SequenceExhaustedError",
    "StorageFullError",
    "TickertapeError",
    "UnsupportedVersionError",
    "UsedAfterForkError",
]

# Report the public location in tracebacks and reprs.
for _name in __all__:
    globals()[_name].__module__ = "tickertape"
del _name
