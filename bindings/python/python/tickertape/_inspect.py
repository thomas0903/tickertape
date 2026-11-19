"""Read-only inspection (checkpoint marker, segment list, full verification) and the writer's reports.

The inspection functions never take the writer lock or modify files, so they are safe to run while a writer is
active. Against a live journal, results describe a moment in time.
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any, NamedTuple, Optional, Union

from . import _native

PathLike = Union[str, "os.PathLike[str]"]


class Checkpoint(NamedTuple):
    """A checkpoint marker slot (same fields as the pure-Python decoder's ``Checkpoint``)."""

    generation: int
    next_seq: int
    """Every record with a smaller sequence number was durable when the slot was written."""
    segment_base_seq: int
    offset: int


class SegmentInfo(NamedTuple):
    """One segment file and its decoded header."""

    path: Path
    base_seq: int
    segment_size: int
    format_version: int
    created_unix_ns: int


class SegmentReport(NamedTuple):
    """What :func:`verify` found in one segment."""

    path: Path
    info: Optional[SegmentInfo]
    """``None`` if the header is invalid."""
    records: int
    end_seq: int
    """Sequence number after the last valid record (the base if there are none)."""
    used_bytes: int
    end: str
    """``"sealed"``, ``"open"`` (the newest segment) or ``"damaged"``."""


class Problem(NamedTuple):
    """A problem found by :func:`verify`."""

    path: Path
    offset: int
    reason: str
    """A stable code, as in :attr:`CorruptJournalError.reason`."""
    details: dict[str, Any]
    message: str


class VerifyReport(NamedTuple):
    """The result of :func:`verify`."""

    records: int
    first_seq: Optional[int]
    last_seq: Optional[int]
    segments: list[SegmentReport]
    problems: list[Problem]

    @property
    def ok(self) -> bool:
        """True if no problems were found."""
        return not self.problems


class RecoveryAction(NamedTuple):
    """One repair performed when a :class:`Writer` opened the journal. Fields that don't apply to ``kind`` are
    ``None``."""

    kind: str
    """``"created"``, ``"zeroed_uncommitted"``, ``"repaired_tail"``, ``"quarantined_segment"``, ``"sealed"``,
    ``"completed_roll"`` or ``"removed_temp"``."""
    segment: Optional[Path] = None
    """The segment concerned (for ``"quarantined_segment"``, where it was)."""
    offset: Optional[int] = None
    """First affected byte offset (``"zeroed_uncommitted"``, ``"repaired_tail"``)."""
    length: Optional[int] = None
    """Number of bytes zeroed or removed (``"zeroed_uncommitted"``, ``"repaired_tail"``)."""
    quarantine_file: Optional[Path] = None
    """Where the removed bytes or segment went (``"repaired_tail"``, ``"quarantined_segment"``)."""
    reason: Optional[str] = None
    """Why, as a :attr:`CorruptJournalError.reason` code (``"repaired_tail"``, ``"quarantined_segment"``)."""
    file: Optional[Path] = None
    """The removed temporary file (``"removed_temp"``)."""


class RecoveryReport(NamedTuple):
    """What recovery found and did when a :class:`Writer` opened the journal (:attr:`Writer.recovery_report`)."""

    next_seq: int
    """Sequence number of the next record after recovery."""
    scanned_from_seq: int
    """Where validation started: the checkpoint marker's ``next_seq``, or the oldest segment's ``base_seq``."""
    had_checkpoint: bool
    """Whether a valid checkpoint marker was found."""
    actions: list[RecoveryAction]
    """Every repair, in order. Empty for a cleanly closed journal."""


class RetentionReport(NamedTuple):
    """Cumulative retention removals since the writer opened (:attr:`Writer.retention_report`)."""

    removed_segments: int
    removed_bytes: int
    already_removed_segments: int
    """Segments that were already gone when retention tried to remove them (removed by someone else)."""


def read_checkpoint(path: PathLike) -> Optional[Checkpoint]:
    """Read the journal's checkpoint marker; ``None`` if it is missing or neither slot is valid."""
    slot = _native.read_checkpoint(os.fspath(path))
    return None if slot is None else Checkpoint(*slot)


def list_segments(path: PathLike) -> list[SegmentInfo]:
    """List the segments, oldest first. Raises :class:`JournalNotFoundError` if there are none."""
    return [SegmentInfo(Path(p), *rest) for p, *rest in _native.list_segments(os.fspath(path))]


def verify(path: PathLike) -> VerifyReport:
    """Scan every segment, checking headers, records, seals and the chain (``tickertape verify``).

    Corruption is reported in :attr:`VerifyReport.problems`, not raised. Releases the GIL while scanning.
    """
    raw = _native.verify(os.fspath(path))
    segments = []
    for seg_path, info, records, end_seq, used_bytes, end in raw["segments"]:
        decoded = None if info is None else SegmentInfo(Path(info[0]), *info[1:])
        segments.append(SegmentReport(Path(seg_path), decoded, records, end_seq, used_bytes, end))
    problems = [Problem(Path(p), offset, reason, details, message)
                for p, offset, reason, details, message in raw["problems"]]
    return VerifyReport(raw["records"], raw["first_seq"], raw["last_seq"], segments, problems)
