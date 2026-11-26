"""Operational metrics (ADR 0032): the journal status sample that ``tickertape status`` prints, the writer's metrics
and a reader's lag.

Nothing reported is guessed: a value the sample could not establish is ``None``, often with a reason code. Durations
are integer nanoseconds (``*_ns``), like the timestamps. What each value means, and its accuracy, is in
https://github.com/thomas0903/tickertape/blob/main/docs/operations.md and ADR 0032.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, NamedTuple, Optional, Union

from . import _native
from ._inspect import Checkpoint

PathLike = Union[str, "os.PathLike[str]"]


class Lag(NamedTuple):
    """How far a reader is behind what it could read now."""

    records: int
    """Records it could return now but has not (for a durable-only reader, records covered by the checkpoint)."""
    bytes: Optional[int]
    """Segment-file footprint up to its target (headers, seals and unused tails count), or ``None`` if unknown."""


class Head(NamedTuple):
    """The writer's head: where the next record goes, as far as the newest segment shows."""

    next_seq: int
    segment_base_seq: int
    offset: int
    segment_size: int
    end: str
    """``"open"`` (the normal state), ``"sealed"`` (a writer died during a roll) or ``"invalid"`` (corruption);
    ``"other"`` stands for a state this version does not name (treat it as unknown, not as healthy)."""
    invalid_reason: Optional[str]
    """For ``"invalid"``: a reason code, as in :attr:`CorruptJournalError.reason`."""
    last_local_ts_ns: Optional[int]
    verified: bool
    """True if every record from the checkpoint to the head was validated. False with no corruption reported does not
    mean that a writer would open the journal."""
    chain_broken: bool
    """True if the segment listing alone proves a segment missing: a writer refuses the journal (corruption)."""


class Segments(NamedTuple):
    """The segment files."""

    segment_count: int
    """Number of segment files (``count`` would hide :meth:`tuple.count`)."""
    first_seq: int
    total_bytes: int
    oldest_created_unix_ns: Optional[int]
    before_checkpoint: int


class CheckpointStatus(NamedTuple):
    """The current checkpoint marker and an estimate of its age."""

    generation: int
    next_seq: int
    segment_base_seq: int
    offset: int
    problem: Optional[str]
    """Why a writer would refuse the marker (corruption): ``"missing_segment"``, ``"invalid_segment_header"``,
    ``"invalid_offset"`` or (from 1.1) ``"records_missing"``; ``None`` if it is usable. Treat any other string as a
    problem too: the list may grow in a minor release."""
    modified_unix_ns: Optional[int]
    estimated_age_ns: Optional[int]
    age_unknown: Optional[str]
    """Why ``estimated_age_ns`` is ``None``: ``"no_modification_time"``, ``"modification_time_in_future"``,
    ``"modification_time_before_segment"``, ``"marker_resized"``, ``"marker_changing"`` (a writer checkpointing very
    often) or ``"older_slot_lost"``."""


class Exposure(NamedTuple):
    """What a power loss now could remove: everything after the checkpoint marker."""

    records: Optional[int]
    bytes: Optional[int]
    estimated_age_ns: Optional[int]
    """An upper bound on how long the uncovered records have been exposed (0 if there are none)."""
    oldest_local_ts_ns: Optional[int]
    verified: bool


class Storage(NamedTuple):
    """Space on the journal's filesystem. The bytes are observed; the rolls are an estimate (optimistic on btrfs)."""

    available_bytes: int
    total_bytes: int
    next_segment_size: int
    spare_segment: bool
    estimated_rolls_left: int


class BeaconReport(NamedTuple):
    """What a reader beacon said at its last write."""

    pid: int
    next_seq: int
    slot_seq: int
    segment_base_seq: int
    offset: int
    seek_pending: bool
    durable_only: bool
    updated_unix_ns: int


class BeaconStatus(NamedTuple):
    """A reader in another process, from its beacon."""

    name: str
    path: Path
    state: str
    """``"live"``, ``"stale"`` (its owner died: history, not lag), ``"uncertain"``, ``"invalid"``, ``"unreadable"`` or
    ``"changing"``."""
    report: Optional[BeaconReport]
    update_age_ns: Optional[int]
    lag: Optional[Lag]
    lag_unknown: Optional[str]
    """Why ``lag`` is ``None``: ``"segment_gone"``, ``"other_history"``, ``"segment_unreadable"``,
    ``"checkpoint_unusable"`` or ``"ahead_of_sample"``."""
    oldest_unread_local_ts_ns: Optional[int]
    unread_age_ns: Optional[int]


class WriterMetrics(NamedTuple):
    """What only the writer knows, from :meth:`Writer.metrics`."""

    next_seq: int
    segment_base_seq: int
    offset: int
    segment_size: int
    checkpoint: Optional[Checkpoint]
    """The marker as this writer last wrote it or read it at open."""
    since_checkpoint_ns: Optional[int]
    """Time since this writer's last successful checkpoint (monotonic clock); ``None`` before its first."""
    uncheckpointed_records: int
    uncheckpointed_bytes: int
    poisoned: bool
    """True after a failed checkpoint: later checkpoints raise :class:`DurabilityLostError`."""
    retention_failures: int


class ReaderMetrics(NamedTuple):
    """A reader's cursor and its lag behind a :class:`JournalStatus`, from :meth:`Reader.metrics`."""

    next_seq: int
    """The next record :meth:`Reader.next` returns (as :attr:`Reader.position`)."""
    slot_seq: int
    """The sequence number at the slot the core reader reads next: ``next_seq + buffered`` unless a seek is pending."""
    segment_base_seq: int
    offset: int
    seek_pending: bool
    durable_only: bool
    buffered: int
    """Records read from the journal but not yet returned; they count in ``lag``, and make its ``bytes`` unknown."""
    lag: Optional[Lag]
    lag_unknown: Optional[str]
    """Why ``lag`` is ``None`` (codes as for :attr:`BeaconStatus.lag_unknown`)."""


@dataclass(frozen=True)
class JournalStatus:
    """One read-only sample of a journal's operational state, from :meth:`JournalStatus.sample`. The values never change; pass it to
    :meth:`Reader.metrics` for a reader's lag as of the sample."""

    path: Path
    sampled_unix_ns: int
    """The observer's wall clock when the sample was taken; ages are measured from it."""
    head: Head
    segments: Segments
    checkpoint: Optional[CheckpointStatus]
    """``None`` if there is no valid marker."""
    exposure: Exposure
    storage: Storage
    writer_pid: Optional[int]
    """The PID the most recent writer recorded in ``writer.lock`` (informational; it stays after the writer exits)."""
    writer_alive: Optional[bool]
    readers_dir: Path
    readers_dir_state: str
    """``"listed"``, ``"missing"`` or ``"unusable"``; ``"other"`` stands for a state this version does not name."""
    readers: list[BeaconStatus]
    """Reader beacons, sorted by name."""
    _sample: Any = field(repr=False, compare=False)

    @staticmethod
    def sample(path: PathLike, readers_dir: Optional[PathLike] = None) -> JournalStatus:
        """Takes one read-only sample of the journal in ``path``: what ``tickertape status`` prints. Needs no writer and
        disturbs none. Beacons are read from ``readers_dir`` (default ``path/readers``).

        Cost: a listing of the directory, the checkpoint marker, a scan of the newest segment from the checkpoint (so
        it grows with what was appended since the last checkpoint), two segment headers, ``statvfs``, ``writer.lock``
        and the beacons. Segments between the checkpoint's and the newest are listed, not read. Runs with the GIL
        released.

        Raises :class:`JournalNotFoundError` if there are no segments, :class:`CorruptJournalError` if the newest
        segment's header is invalid, :class:`UnsupportedVersionError` or :class:`JournalIOError`.
        """
        sample = _native.sample_status(os.fspath(path), None if readers_dir is None else os.fspath(readers_dir))
        return JournalStatus(**sample.fields(), _sample=sample)
