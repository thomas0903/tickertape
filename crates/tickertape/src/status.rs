//! Operational metrics: how far the writer has got, how much a power loss would cost now, how much room is left on
//! the volume, and how far behind each reader is (ADR 0032; `docs/operations.md` → Monitoring).
//!
//! Everything here is read-only and safe to call from any process while a writer is active. Nothing takes the writer
//! lock, starts a thread, keeps global state or talks to the network, and nothing here adds work to
//! [`Writer::append`](crate::Writer::append) or to [`Reader`]'s read path. The cost of each call is documented on it.
//!
//! - [`status`] (or a [`Sampler`], which is cheaper when called repeatedly) samples a journal from any process.
//! - [`Writer::metrics`](crate::Writer::metrics) reports what only the writer knows, without system calls.
//! - [`Reader::cursor`] and [`Status::lag`] give the lag of a reader in this process.
//! - A [`ReaderBeacon`] makes a reader in another process visible to [`status`]. It is opt-in, outside the on-disk
//!   format, and never read by the writer, by other readers or by retention.
//!
//! **What is reported is never guessed.** A value that a sample could not establish is `None` (often with a reason
//! next to it), and values that are estimates say so in their names (`estimated_*`). A sample that finds the
//! journal in a state the writer would refuse to open (a checkpoint marker that points to a missing segment or an
//! impossible offset, or bytes at the head that fail validation) reports it as a [`Breach`] that is always checked.
//! A sample does not read every segment, so it cannot see everything [`Writer::open`](crate::Writer::open) checks:
//! [`Head::verified`] and [`Exposure::verified`] say whether it validated the chain from the checkpoint to the head.
//!
//! Exporting to a metrics system takes a few lines of application code; see `docs/operations.md` → Monitoring for
//! Prometheus and OpenTelemetry examples. The CLI's `tickertape status` prints the same values as text, JSON or
//! Prometheus text format.
//!
//! ```no_run
//! # fn main() -> tickertape::Result<()> {
//! let status = tickertape::status::status("/tmp/journal")?;
//! println!(
//!     "next seq {}, {:?} records not yet durable, about {} more segments fit on the volume",
//!     status.head.position.next_seq, status.exposure.records, status.storage.estimated_rolls_left,
//! );
//! # Ok(())
//! # }
//! ```

mod beacon;

pub use beacon::ReaderBeacon;

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::error::{CorruptReason, Error, Result};
use crate::format::{self, CheckpointSlot, RecordHeader, SegmentHeader, Slot};
use crate::journal::{self, Identity, PreadView};
use crate::reader::Reader;

/// Name of the default beacon directory inside a journal directory (`<dir>/readers`).
pub const READERS_DIR: &str = "readers";
/// Default minimum time between two beacon writes (see [`ReaderBeacon::set_interval`]).
pub const DEFAULT_BEACON_INTERVAL: Duration = Duration::from_secs(1);

/// Re-reads of a slot that fails validation before it counts as invalid (the copy may predate the commit).
const VALIDATION_RETRIES: u32 = 3;
/// Bytes fetched by one `pread` when reading a single record (more if the record is larger).
const RECORD_WINDOW: usize = 4096;
/// Attempts at a sample when a file disappears under it (retention or recovery moved it).
const SAMPLE_ATTEMPTS: u32 = 3;
/// Times a sample re-lists the directory because the newest segment it scanned turned out sealed with a successor
/// (the writer rolled during the sample) before it gives up.
const ROLL_ATTEMPTS: u32 = 16;
/// Reads of the checkpoint marker while its metadata keeps changing under the read.
const MARKER_ATTEMPTS: u32 = 3;
/// Allowance for coarse filesystem timestamps when the marker's modification time is compared with the creation time
/// in its segment's header (Linux stamps files from a clock that can lag the wall clock by a scheduler tick).
const MTIME_SLACK_NS: u64 = 2_000_000_000;

/// A position in the journal: the sequence number expected there, and where that is in the segment files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Position {
    /// Sequence number of the record at this position (or that the next record will get).
    pub next_seq: u64,
    /// Base sequence number of the segment the position is in.
    pub segment_base_seq: u64,
    /// Byte offset in that segment.
    pub offset: u64,
}

impl Position {
    /// A position from its parts.
    pub fn new(next_seq: u64, segment_base_seq: u64, offset: u64) -> Position {
        Position {
            next_seq,
            segment_base_seq,
            offset,
        }
    }
}

/// A reader's cursor, from [`Reader::cursor`]: what [`Status::lag`] and [`ReaderBeacon`] need to place it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Cursor {
    /// Sequence number of the next record the reader returns ([`Reader::position`]).
    pub next_seq: u64,
    /// The slot the reader reads next, and the sequence number expected there. Equal to `next_seq` unless
    /// `seek_pending`.
    pub position: Position,
    /// True while the target of a [`Reader::seek`] is still ahead of `position`: the reader is skipping records on
    /// its way there, or waiting for the target to be written. `position` is then where it is skipping from, so its
    /// byte lag and unread age are unknown.
    pub seek_pending: bool,
    /// The reader was opened with [`ReaderConfig::durable_only`](crate::ReaderConfig::durable_only), so it never
    /// passes the checkpoint marker, and its lag is measured against the checkpoint rather than the head.
    pub durable_only: bool,
    /// The segment file the reader has open (device, inode). The reader keeps it open, so it can't be reused.
    file: Identity,
}

/// The reader's cursor, as [`Reader`] hands it to this module.
pub(crate) struct CursorParts<'r> {
    /// Sequence number expected at `offset`.
    pub(crate) slot_seq: u64,
    /// Records before this are skipped (a pending seek).
    pub(crate) skip_until: u64,
    pub(crate) path: &'r Path,
    pub(crate) offset: u64,
    pub(crate) file: &'r File,
    pub(crate) identity: Identity,
    pub(crate) durable_only: bool,
}

impl CursorParts<'_> {
    fn cursor(&self) -> Cursor {
        let base = self
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(format::parse_segment_file_name)
            .unwrap_or(self.slot_seq);
        Cursor {
            next_seq: self.slot_seq.max(self.skip_until),
            position: Position::new(self.slot_seq, base, self.offset),
            seek_pending: self.skip_until > self.slot_seq,
            durable_only: self.durable_only,
            file: self.identity,
        }
    }
}

/// How the newest segment ends, as seen by the sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HeadEnd {
    /// At a slot with no record yet: the normal state.
    Open,
    /// At a seal whose successor did not exist when the sample checked: a writer died during a roll. The next writer
    /// creates the successor (`docs/format.md` → Recovery).
    Sealed,
    /// At bytes that still failed validation after re-reading. Not possible after a writer process crash; seen after
    /// a power loss or storage corruption, until the next writer repairs it.
    Invalid(CorruptReason),
}

/// The writer's head: where the next record will go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Head {
    /// Where the next record will be written, as far as the newest segment shows.
    pub position: Position,
    /// Size of the newest segment, in bytes.
    pub segment_size: u64,
    /// What the sample found at `position`.
    pub end: HeadEnd,
    /// `local_ts_ns` of the last record the scan passed in the newest segment, if it passed one and its timestamp
    /// is set (non-zero). Caller-supplied data, in the producer's clock. A one-shot sample that starts at a
    /// checkpoint at the head passes no record, so this is `None` there.
    pub last_local_ts_ns: Option<u64>,
    /// True if this sample (or, for a [`Sampler`], its earlier samples of the same segment file) validated every
    /// record from the checkpoint marker's position (without a marker, from the start of the oldest segment) to the
    /// head. False means the head is at invalid bytes ([`HeadEnd::Invalid`], a corruption breach), or the checkpoint
    /// is in an older segment (or there are several segments and no checkpoint): the segments in between were not
    /// read, so a missing or damaged one would not have been noticed, and the head's sequence number is what the
    /// newest segment's name and records imply, not a verified count. A sample with
    /// `verified == false` and no breach does **not** mean that [`Writer::open`](crate::Writer::open) would open the
    /// journal.
    pub verified: bool,
    /// True if the segment listing alone proves a segment missing between the checkpoint (or the oldest segment) and
    /// the head: the next segment's base sequence number is beyond what the previous segment can hold. The writer
    /// refuses such a journal ([`Breach::ChainBroken`]), and no exposure is summed across the gap. Most missing
    /// segments can't be proven this way; they leave `verified` false.
    pub chain_broken: bool,
}

/// The segment files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Segments {
    /// Number of segment files.
    pub count: u64,
    /// Base sequence number of the oldest segment: the first retained record, if it has been written.
    pub first_seq: u64,
    /// Sum of the segment files' sizes (their full, preallocated size).
    pub total_bytes: u64,
    /// Creation time from the oldest segment's header (the writer's wall clock), if its header is readable.
    pub oldest_created_unix_ns: Option<u64>,
    /// Segments older than the checkpoint marker's segment: the ones retention could remove (if sealed and the chain
    /// is intact). Zero without a checkpoint.
    pub before_checkpoint: u64,
}

/// Why the checkpoint marker can't be used: [`Writer::open`](crate::Writer::open) refuses such a journal under every
/// recovery policy (`docs/format.md` → Recovery, step 2), and `tickertape verify` reports it as corruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CheckpointProblem {
    /// The marker's segment doesn't exist.
    MissingSegment,
    /// The marker's segment exists but its header is invalid.
    InvalidSegmentHeader,
    /// The marker's offset is misaligned or outside its segment's record area.
    InvalidOffset,
    /// The records of the marker's segment don't reach the marker's position: the segment doesn't hold, right before
    /// the marker's offset, a valid record with sequence number `next_seq - 1` that ends exactly there (or, at offset
    /// 64, `next_seq` isn't the segment's base). The records the marker vouches for are missing or damaged: segments
    /// restored from a backup older than the marker, or the last covered record damaged (ADR 0056; the writer reports
    /// it as [`CorruptReason::DanglingCheckpoint`] at the segment's path and the marker's offset). Added in 1.1.
    RecordsMissing,
}

impl CheckpointProblem {
    /// A stable snake_case code (used by the CLI).
    pub fn code(self) -> &'static str {
        match self {
            CheckpointProblem::MissingSegment => "missing_segment",
            CheckpointProblem::InvalidSegmentHeader => "invalid_segment_header",
            CheckpointProblem::InvalidOffset => "invalid_offset",
            CheckpointProblem::RecordsMissing => "records_missing",
        }
    }
}

/// Why the checkpoint's age can't be estimated from the marker's modification time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AgeUnknown {
    /// The filesystem doesn't report a modification time.
    NoModificationTime,
    /// The modification time is later than the observer's wall clock: the clock was stepped back, or something set
    /// the time.
    InFuture,
    /// The modification time is earlier than the creation time in the header of the marker's segment, which the
    /// checkpoint can't predate: the journal was copied without keeping times, or a clock was stepped.
    BeforeSegment,
    /// The marker file doesn't have its full length, so its modification time is the time it was cut, not of a
    /// checkpoint. A writer leaves a short marker short until its next checkpoint has succeeded, and extends it only
    /// then.
    MarkerResized,
    /// The marker's metadata kept changing while it was read: a writer checkpointing very often. Not a breach of
    /// [`Thresholds::max_checkpoint_age`], since it means the marker was just written.
    Changing,
    /// The marker's current slot has a generation of 2 or more, but the other slot doesn't hold the previous
    /// generation, which every checkpoint leaves there: the marker lost bytes (it was cut, then extended, for example
    /// by a v0.4.0 writer at open, or a slot write was torn), so its modification time is not known to be a
    /// checkpoint's. Decided from the marker's contents, not its time.
    OlderSlotLost,
}

impl AgeUnknown {
    /// A stable snake_case code (used by the CLI).
    pub fn code(self) -> &'static str {
        match self {
            AgeUnknown::NoModificationTime => "no_modification_time",
            AgeUnknown::InFuture => "modification_time_in_future",
            AgeUnknown::BeforeSegment => "modification_time_before_segment",
            AgeUnknown::MarkerResized => "marker_resized",
            AgeUnknown::Changing => "marker_changing",
            AgeUnknown::OlderSlotLost => "older_slot_lost",
        }
    }
}

/// The current checkpoint marker and an estimate of its age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckpointStatus {
    /// The current slot of the marker (`docs/format.md` → Checkpoint marker).
    pub marker: CheckpointSlot,
    /// Why the marker can't be used, if it can't: the writer refuses the journal, and [`Breach::CheckpointInvalid`]
    /// is reported.
    pub problem: Option<CheckpointProblem>,
    /// The marker file's modification time, in nanoseconds since the Unix epoch, if the filesystem reports one.
    pub modified_unix_ns: Option<u64>,
    /// Estimated time since the last checkpoint: the observer's wall clock minus `modified_unix_ns`, read after the
    /// marker. The writer changes the marker only by writing a slot at a checkpoint, so its modification time is the
    /// checkpoint's time, at filesystem timestamp resolution. It is wrong if something else writes or `touch`es the
    /// file, if the journal was copied without keeping times, or if the observer's clock was stepped forward since.
    /// A sample taken while a checkpoint writes the marker may pair the new time with the previous slot. `None` with
    /// a reason in `age_unknown` when the time is detectably unreliable.
    pub estimated_age: Option<Duration>,
    /// Why `estimated_age` is `None`.
    pub age_unknown: Option<AgeUnknown>,
}

impl CheckpointStatus {
    /// The marker's position.
    pub fn position(&self) -> Position {
        Position::new(
            self.marker.next_seq,
            self.marker.segment_base_seq,
            self.marker.offset,
        )
    }
}

/// What a power loss now could remove: everything after the checkpoint marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Exposure {
    /// Committed records not covered by the checkpoint marker (without a marker, every record from the start of the
    /// oldest segment). `None` if the marker can't be used ([`CheckpointStatus::problem`]), lies beyond the head, a
    /// segment in between is proven missing ([`Head::chain_broken`]), or the head is at invalid bytes
    /// ([`HeadEnd::Invalid`]: what lies before them is not known to be what the writer would keep). If the chain is not verified
    /// ([`Exposure::verified`] false), this is the head's sequence number (as the newest segment implies it) minus the
    /// marker's, whether or not the segments in between are intact.
    pub records: Option<u64>,
    /// Segment-file footprint between the marker's position and the head (see [`Lag::bytes`]); zero when no record
    /// is uncovered. `None` if `records` is, or a segment named by an end is missing. If the chain is not verified,
    /// it sums the segment files listed in between, so a missing one is not counted.
    pub bytes: Option<u64>,
    /// Estimated time the uncovered records have been exposed, at most: zero if there are none, otherwise the
    /// checkpoint's [`estimated_age`](CheckpointStatus::estimated_age) (every uncovered record was appended after
    /// it). `None` if records are or may be uncovered and there is no marker or its age is unknown.
    pub estimated_age: Option<Duration>,
    /// `local_ts_ns` of the oldest uncovered record, if there is one, it could be read, and its timestamp is set.
    /// Producer's clock.
    pub oldest_local_ts_ns: Option<u64>,
    /// True if the sample validated the records between the marker and the head ([`Head::verified`]); `records` and
    /// `bytes` are otherwise what the newest segment implies.
    pub verified: bool,
}

/// Space on the journal's filesystem. The bytes are an observed fact; rolls and time are estimates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Storage {
    /// Bytes available to unprivileged users (`statvfs`: `f_bavail * f_frsize`).
    pub available_bytes: u64,
    /// Size of the filesystem (`f_blocks * f_frsize`).
    pub total_bytes: u64,
    /// Size assumed for the segments still to come: the prepared spare's size if there is one (the writer makes it
    /// with its configured size), otherwise the newest segment's size. Wrong while a writer reopened with a different
    /// segment size has neither rolled nor prepared a spare.
    pub next_segment_size: u64,
    /// True if a prepared next segment (`next-segment.tmp`, ADR 0022) exists with a valid segment size and all its
    /// blocks allocated. Its space is already taken, but publishing it at the next roll can still fail on a full
    /// volume (crash contract C9, C10), so it is not counted in `estimated_rolls_left`.
    pub spare_segment: bool,
    /// Estimated number of further segment rolls that can succeed if nothing else uses the volume:
    /// `available_bytes / next_segment_size`. Writes into the active segment need no new space (crash contract C10),
    /// **except on btrfs**, where a full volume makes them fail; the roll after these fails with a storage-full error
    /// (C9).
    pub estimated_rolls_left: u64,
    /// Estimated time until the writer needs roll `estimated_rolls_left + 1`, at the append rate a [`Sampler`]
    /// measured between its last two samples: the bytes left in the active segment (none if it is sealed) plus
    /// `estimated_rolls_left` segments, divided by the rate. `None` without a rate (the first sample, or after the
    /// history changed) or when nothing was appended.
    pub estimated_time_to_full: Option<Duration>,
}

/// Append rates measured between two samples of a [`Sampler`].
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct Rates {
    /// Time between the two samples (monotonic clock).
    pub interval: Duration,
    /// Records appended per second.
    pub records_per_sec: f64,
    /// Segment-file footprint the head advanced per second. `None` if a segment in between is missing.
    pub bytes_per_sec: Option<f64>,
}

/// The writer process, as far as an outside observer can tell. Best effort and informational: never used for health
/// checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct WriterProcess {
    /// The PID the most recent writer recorded in `writer.lock`. It stays after that writer exits.
    pub pid: Option<u32>,
    /// Whether a process with that PID exists (`kill(pid, 0)`; a PID can be reused). `None` if unknown.
    pub alive: Option<bool>,
}

/// Whether a beacon's owner is still running, and whether the beacon could be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BeaconState {
    /// A process holds the beacon's lock: its owner, or a process that inherited the owner's descriptor through
    /// `fork` (see [`ReaderBeacon`]).
    Live,
    /// Nobody holds the lock: the owner died without removing the file. Its position is history, not lag.
    Stale,
    /// A valid beacon whose lock state can't be established: the filesystem refused the probe (locks unsupported).
    Uncertain,
    /// The file isn't a valid beacon: wrong length, magic, version or checksum, even after re-reading.
    Invalid,
    /// The file couldn't be opened or read (for example, another user's beacon created under umask 077).
    Unreadable,
    /// The name was replaced by another file during every attempt to observe it, so nothing seen belongs to the file
    /// that holds the name now: no contents and no lag are reported.
    Changing,
}

impl BeaconState {
    /// A stable snake_case code (used by the CLI).
    pub fn code(self) -> &'static str {
        match self {
            BeaconState::Live => "live",
            BeaconState::Stale => "stale",
            BeaconState::Uncertain => "uncertain",
            BeaconState::Invalid => "invalid",
            BeaconState::Unreadable => "unreadable",
            BeaconState::Changing => "changing",
        }
    }
}

/// Whether the beacon directory could be listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BeaconDirState {
    /// Listed (possibly empty).
    Listed,
    /// It doesn't exist: no reader has created a beacon there.
    Missing,
    /// It exists but couldn't be listed (not a directory, no permission, an I/O error). No readers are reported.
    Unusable(io::ErrorKind),
}

/// Lag of a reader: how far it is behind what it could read now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Lag {
    /// Records the reader could return now but hasn't: committed records, or for a durable-only reader, records
    /// covered by the checkpoint marker.
    pub records: u64,
    /// Segment-file footprint between the reader's cursor and its target (the head, or the marker's position for a
    /// durable-only reader): the rest of the cursor's segment, every segment in between and the target's offset, so
    /// headers, seals, unused tails and trailers count. Per segment boundary it exceeds the unread record bytes by
    /// less than 72 bytes plus the size of the first record of the next segment (the record that didn't fit). Zero
    /// when `records` is. `None` while a seek is pending ([`Cursor::seek_pending`]) or if the segment of either end is
    /// not listed. The segments in between are the ones listed, not read: unless the sample verified the chain
    /// ([`Head::verified`]), a missing one is simply not counted.
    pub bytes: Option<u64>,
}

/// Why a reader's lag is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LagUnknown {
    /// The cursor's segment is no longer in the journal: retention removed it, or recovery moved it to `quarantine/`.
    SegmentGone,
    /// The segment with the cursor's base sequence number is a different file: the beacon belongs to another journal
    /// sharing the beacon directory, the journal was replaced, or recovery repaired the segment after the reader
    /// read it.
    OtherHistory,
    /// The cursor's segment exists but its header couldn't be read.
    SegmentUnreadable,
    /// A durable-only reader, and the checkpoint marker can't be used ([`CheckpointStatus::problem`]).
    CheckpointUnusable,
    /// The cursor's segment is newer than every segment the sample listed: the writer created it after the sample
    /// (take a new sample).
    AheadOfSample,
}

impl LagUnknown {
    /// A stable snake_case code (used by the CLI).
    pub fn code(self) -> &'static str {
        match self {
            LagUnknown::SegmentGone => "segment_gone",
            LagUnknown::OtherHistory => "other_history",
            LagUnknown::SegmentUnreadable => "segment_unreadable",
            LagUnknown::CheckpointUnusable => "checkpoint_unusable",
            LagUnknown::AheadOfSample => "ahead_of_sample",
        }
    }
}

/// What a beacon says about its reader, at its last write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct BeaconReport {
    /// The owner's PID, as it recorded it (informational).
    pub pid: u32,
    /// Sequence number of the next record the reader returns.
    pub next_seq: u64,
    /// The slot it reads next ([`Cursor::position`]).
    pub position: Position,
    /// A seek was pending ([`Cursor::seek_pending`]).
    pub seek_pending: bool,
    /// The reader is durable-only ([`Cursor::durable_only`]).
    pub durable_only: bool,
    /// The owner's wall-clock time of the write, nanoseconds since the Unix epoch.
    pub updated_unix_ns: u64,
}

/// A reader in another process, from its [`ReaderBeacon`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReaderStatus {
    /// The beacon's name (its file name).
    pub name: String,
    /// The beacon file.
    pub path: PathBuf,
    /// Whether its owner is running, and whether the file could be read.
    pub state: BeaconState,
    /// The beacon's contents, unless the state is [`BeaconState::Invalid`], [`BeaconState::Unreadable`] or
    /// [`BeaconState::Changing`].
    pub report: Option<BeaconReport>,
    /// Time since the last beacon write, by the observer's wall clock. `None` if the write time is in the future.
    pub update_age: Option<Duration>,
    /// Lag as of the last beacon write, if it could be established for this journal.
    pub lag: Option<Lag>,
    /// Why `lag` is `None` for a beacon that could be read.
    pub lag_unknown: Option<LagUnknown>,
    /// `local_ts_ns` of the first record the reader has not returned, if it is committed, could be read, and its
    /// timestamp is set.
    pub oldest_unread_local_ts_ns: Option<u64>,
    /// Observer's wall clock minus `oldest_unread_local_ts_ns`, or zero if the reader has caught up. Meaningful when
    /// the producer stamps `local_ts_ns` from this host's wall clock (for example with
    /// [`crate::clock::realtime_ns`]). `None` while a seek is pending, if the lag or the timestamp is unknown, or if
    /// the timestamp is later than the observer's clock.
    pub unread_age: Option<Duration>,
}

/// One sample of a journal's operational state. See [`status`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Status {
    /// The journal directory, as given.
    pub dir: PathBuf,
    /// The observer's wall clock after reading the checkpoint marker and the beacons, nanoseconds since the Unix
    /// epoch. Ages are measured from it.
    pub sampled_unix_ns: u64,
    /// The writer's head.
    pub head: Head,
    /// The segment files.
    pub segments: Segments,
    /// The current checkpoint marker, or `None` if there is no valid one.
    pub checkpoint: Option<CheckpointStatus>,
    /// What a power loss now could remove.
    pub exposure: Exposure,
    /// Space on the journal's filesystem.
    pub storage: Storage,
    /// Append rates since the previous sample of the same [`Sampler`], or `None`.
    pub rates: Option<Rates>,
    /// The writer process (best effort).
    pub writer: WriterProcess,
    /// The beacon directory that was read.
    pub readers_dir: PathBuf,
    /// Whether it could be listed.
    pub readers_dir_state: BeaconDirState,
    /// Readers with a beacon in the beacon directory, sorted by name.
    pub readers: Vec<ReaderStatus>,
    /// Temporary names of segments that interrupted removals by an archiver left in the journal directory
    /// (`{base:020}.tape.{pid}.{unix_ns}.removing`, ADR 0055), sorted. Such a file may hold records the journal no longer
    /// has; a retention run unlinks it only once newer records have been removed (`docs/operations.md`).
    pub removal_leftovers: Vec<String>,
    /// Every segment file, for [`Status::lag`].
    files: Vec<SegFile>,
}

/// What only the writer knows, from [`Writer::metrics`](crate::Writer::metrics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct WriterMetrics {
    /// Where the next record will be written.
    pub head: Position,
    /// Size of the active segment.
    pub segment_size: u64,
    /// The checkpoint marker as this writer last wrote it, or read it at open. `None` if there is none.
    pub checkpoint: Option<CheckpointSlot>,
    /// Time since this writer's last successful checkpoint, by the monotonic clock (immune to wall-clock steps).
    /// `None` if this writer hasn't checkpointed yet; [`status`] estimates the marker's age from its modification
    /// time.
    pub since_checkpoint: Option<Duration>,
    /// Records appended but not covered by the checkpoint marker (with no marker, every record from the start of the
    /// oldest segment when the writer opened).
    pub uncheckpointed_records: u64,
    /// Bytes of records and seals appended after the marker (by this writer, or before it opened by earlier writers),
    /// in every segment: what a power loss could remove. The next checkpoint syncs at least these (whole files for
    /// segments an earlier writer left).
    pub uncheckpointed_bytes: u64,
    /// True after a failed checkpoint (also as soon as a background checkpoint, or the early write-back of segment
    /// data on Linux, has failed): later checkpoints fail with [`Error::DurabilityLost`] (the first one after a
    /// failure not yet returned fails with that failure's error).
    pub poisoned: bool,
    /// Retention runs that failed since open. The last error is [`Writer::last_retention_error`](crate::Writer::last_retention_error).
    pub retention_failures: u64,
}

/// Health thresholds for [`Status::breaches`]. All are off by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Thresholds {
    /// Breached if records are (or may be) uncovered and the checkpoint's estimated age is greater than this or
    /// unknown, or there is no checkpoint. An idle journal whose records are all covered is healthy however old its
    /// checkpoint is, and so is an empty journal. An age unknown because the marker was rewritten during every read
    /// ([`AgeUnknown::Changing`]: a writer checkpointing very often) is not a breach.
    pub max_checkpoint_age: Option<Duration>,
    /// Breached if fewer bytes than this are available on the volume.
    pub min_headroom_bytes: Option<u64>,
    /// Breached if fewer than this many more rolls are estimated to fit on the volume.
    pub min_headroom_segments: Option<u64>,
    /// Breached if a live (or [uncertain](BeaconState::Uncertain)) reader beacon lags by more than this many records.
    pub max_reader_lag: Option<u64>,
}

impl Thresholds {
    /// Sets [`Thresholds::max_checkpoint_age`].
    pub fn max_checkpoint_age(mut self, age: Duration) -> Self {
        self.max_checkpoint_age = Some(age);
        self
    }

    /// Sets [`Thresholds::min_headroom_bytes`].
    pub fn min_headroom_bytes(mut self, bytes: u64) -> Self {
        self.min_headroom_bytes = Some(bytes);
        self
    }

    /// Sets [`Thresholds::min_headroom_segments`].
    pub fn min_headroom_segments(mut self, segments: u64) -> Self {
        self.min_headroom_segments = Some(segments);
        self
    }

    /// Sets [`Thresholds::max_reader_lag`].
    pub fn max_reader_lag(mut self, records: u64) -> Self {
        self.max_reader_lag = Some(records);
        self
    }
}

/// A health check that failed. See [`Status::breaches`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Breach {
    /// The head is at invalid bytes ([`HeadEnd::Invalid`]). Always checked; corruption.
    HeadInvalid,
    /// The checkpoint marker can't be used ([`CheckpointStatus::problem`]): the writer refuses to open the journal.
    /// Always checked; corruption.
    CheckpointInvalid,
    /// A segment between the checkpoint and the head is missing, as the listing proves ([`Head::chain_broken`]): the
    /// writer refuses to open the journal. Always checked; corruption.
    ChainBroken,
    /// [`Thresholds::max_checkpoint_age`].
    CheckpointAge,
    /// [`Thresholds::min_headroom_bytes`].
    HeadroomBytes,
    /// [`Thresholds::min_headroom_segments`].
    HeadroomSegments,
    /// [`Thresholds::max_reader_lag`], for the named reader.
    ReaderLag {
        /// The beacon's name.
        name: String,
    },
}

impl Breach {
    /// A stable snake_case code for the breach (used by the CLI's JSON output).
    pub fn code(&self) -> &'static str {
        match self {
            Breach::HeadInvalid => "head_invalid",
            Breach::CheckpointInvalid => "checkpoint_invalid",
            Breach::ChainBroken => "chain_broken",
            Breach::CheckpointAge => "checkpoint_age",
            Breach::HeadroomBytes => "headroom_bytes",
            Breach::HeadroomSegments => "headroom_segments",
            Breach::ReaderLag { .. } => "reader_lag",
        }
    }

    /// True if the sample found the journal corrupt, as `tickertape verify` or [`Writer::open`](crate::Writer::open)
    /// would (the CLI exits with 2 rather than 3).
    pub fn is_corruption(&self) -> bool {
        matches!(
            self,
            Breach::HeadInvalid | Breach::CheckpointInvalid | Breach::ChainBroken
        )
    }
}

impl Status {
    /// Lag of a reader in this process, whose cursor is `cursor` ([`Reader::cursor`]), behind this sample's head (or,
    /// for a durable-only reader, behind its checkpoint marker; with no marker such a reader can read nothing, so its
    /// lag is zero).
    ///
    /// The cursor's segment must be the file this sample listed under that name (same device and inode; the reader
    /// keeps its file open, so the inode can't have been reused).
    ///
    /// Cost: no system calls; a search of the segment list.
    ///
    /// # Errors
    ///
    /// [`LagUnknown`] if the lag can't be established.
    pub fn lag(&self, cursor: &Cursor) -> std::result::Result<Lag, LagUnknown> {
        let file = self.listed(cursor.position.segment_base_seq)?;
        if file.file != cursor.file {
            return Err(LagUnknown::OtherHistory);
        }
        self.lag_from(
            cursor.next_seq,
            cursor.position,
            cursor.seek_pending,
            cursor.durable_only,
        )
    }

    /// The listed segment `base`; if there is none, why: gone, or created after the sample.
    fn listed(&self, base: u64) -> std::result::Result<&SegFile, LagUnknown> {
        self.files.iter().find(|f| f.base == base).ok_or_else(|| {
            if self.files.last().is_some_and(|f| base > f.base) {
                LagUnknown::AheadOfSample
            } else {
                LagUnknown::SegmentGone
            }
        })
    }

    fn lag_from(
        &self,
        next_seq: u64,
        position: Position,
        seek_pending: bool,
        durable_only: bool,
    ) -> std::result::Result<Lag, LagUnknown> {
        let target = if durable_only {
            match &self.checkpoint {
                None => {
                    return Ok(Lag {
                        records: 0,
                        bytes: Some(0),
                    });
                }
                Some(c) if c.problem.is_some() => return Err(LagUnknown::CheckpointUnusable),
                Some(c) => c.position(),
            }
        } else {
            self.head.position
        };
        let records = target.next_seq.saturating_sub(next_seq);
        let bytes = if records == 0 {
            Some(0)
        } else if seek_pending {
            None
        } else {
            footprint(&self.files, position, target)
        };
        Ok(Lag { records, bytes })
    }

    /// The health checks this sample fails, given `thresholds`. [`Breach::HeadInvalid`],
    /// [`Breach::CheckpointInvalid`] and [`Breach::ChainBroken`] are always checked.
    ///
    /// The rule for breaks in the chain: a break the sample confirms from what it reads anyway (the head at invalid
    /// bytes, a marker the writer refuses, a segment the listing proves missing) is a corruption breach, and no
    /// exposure is summed across it. What the sample doesn't read (the segments between an older checkpoint and the
    /// head) is not checked: no breach, but [`Head::verified`] is false, so an empty list doesn't then mean that the
    /// writer would open the journal.
    ///
    /// Cost: no system calls.
    pub fn breaches(&self, thresholds: &Thresholds) -> Vec<Breach> {
        let mut out = Vec::new();
        if matches!(self.head.end, HeadEnd::Invalid(_)) {
            out.push(Breach::HeadInvalid);
        }
        if self.checkpoint.is_some_and(|c| c.problem.is_some()) {
            out.push(Breach::CheckpointInvalid);
        }
        if self.head.chain_broken {
            out.push(Breach::ChainBroken);
        }
        if let Some(max) = thresholds.max_checkpoint_age {
            // A marker rewritten during every read was checkpointed moments ago: its age is unknown but tiny.
            let just_written = self.checkpoint.is_some_and(|c| {
                c.problem.is_none() && c.age_unknown == Some(AgeUnknown::Changing)
            });
            if !just_written
                && self.exposure.records != Some(0)
                && self.exposure.estimated_age.is_none_or(|age| age > max)
            {
                out.push(Breach::CheckpointAge);
            }
        }
        if thresholds
            .min_headroom_bytes
            .is_some_and(|min| self.storage.available_bytes < min)
        {
            out.push(Breach::HeadroomBytes);
        }
        if thresholds
            .min_headroom_segments
            .is_some_and(|min| self.storage.estimated_rolls_left < min)
        {
            out.push(Breach::HeadroomSegments);
        }
        if let Some(max) = thresholds.max_reader_lag {
            for reader in &self.readers {
                if matches!(reader.state, BeaconState::Live | BeaconState::Uncertain)
                    && reader.lag.is_some_and(|l| l.records > max)
                {
                    out.push(Breach::ReaderLag {
                        name: reader.name.clone(),
                    });
                }
            }
        }
        out
    }
}

/// Samples the operational state of the journal in `dir`: head, checkpoint and exposure, storage headroom, the
/// writer process, and readers that publish a beacon in `<dir>/readers`. Read-only; safe while a writer is active.
///
/// A sample is a set of observations made one after the other, not an atomic snapshot. If a file disappears while
/// it is taken (retention or recovery moved it), the sample is retried, up to 3 attempts. Every file is opened
/// non-blocking and must be a regular file, so a FIFO or device in the journal can't hang it.
///
/// **Cost** of one call, in system calls, on the normal path (every `open` is followed by a `close`; a `readdir` pass
/// is `open`, `fstat`, two `getdents64` and `close`; ADR 0032 → "Sampling cost" also lists the retries):
///
/// - the journal directory: a `readdir` pass and one `stat` per segment file;
/// - the checkpoint marker: `open`, three `fstat`s and two 64-byte `pread`s (the reads are repeated, at most 3 times,
///   while its metadata changes; the listing and the marker are read again, at most 3 times, if the marker names a
///   segment the listing lacks);
/// - the newest segment: `open`, three `fstat`s, a 64-byte header `pread`, and a `pread` scan in 1 MiB windows
///   **from the checkpoint position if the marker points into it, otherwise from its start**, so at most one segment.
///   If the scan ends at a seal (an `lstat` of the successor) whose successor exists (the writer rolled meanwhile),
///   the directory is listed and the marker read again and the new newest segment scanned instead, up to 16 times;
/// - the checkpoint's segment and the oldest segment, if they are not the newest: `open`, three `fstat`s and a
///   header `pread` each;
/// - in the checkpoint's segment, the record right before the marker's position, checked as
///   [`Writer::open`](crate::Writer::open) checks it (ADR 0056): one `pread` of 4 KiB, or more for a longer record
///   (from that record's start to the marker's offset); back to the segment's first slot when the records don't reach
///   the position ([`CheckpointProblem::RecordsMissing`]);
/// - with uncovered records whose first one wasn't passed by the scan: one record lookup in the checkpoint's
///   segment (one `pread` of 4 KiB, or of the whole record if it is larger, since it is validated; following one
///   seal costs an `open`, three `fstat`s, a header `pread` and another lookup);
/// - `statvfs` on the directory, `lstat` of `next-segment.tmp`, `open`, `fstat` and two `read`s of `writer.lock`,
///   and `kill(pid, 0)`;
/// - the beacon directory: a `readdir` pass, and per beacon `open`, `fstat`, two `pread`s (129 bytes, then the end
///   of the file), one `flock` (held) or two (free: probe and release) and an `lstat` (more if the file is replaced
///   or fails its checksum while it is read); per beacon whose segment isn't already open, `open`, three `fstat`s
///   and a header `pread`; and per lagging beacon, one record lookup;
/// - one monotonic and one wall-clock read (no system call where the vDSO provides them).
///
/// No file stays open after the call. Use a [`Sampler`] to sample repeatedly: it scans only what was appended since
/// its previous sample.
///
/// # Errors
///
/// [`Error::NotFound`] if `dir` has no segments; [`Error::Corrupt`] if it has none but the checkpoint marker names
/// one (the writer refuses it as a dangling checkpoint), or if the newest segment's header is invalid
/// ([`Error::UnsupportedVersion`] for an unsupported version); [`Error::Io`], including when the writer rolled 16 times during one sample. A beacon
/// that can't be read, or a beacon directory that can't be listed, never fails the sample.
pub fn status(dir: impl AsRef<Path>) -> Result<Status> {
    Sampler::new(dir).sample()
}

/// Samples a journal repeatedly, more cheaply than [`status`]: each sample resumes the head scan where the previous
/// one stopped, and successive samples give append [`Rates`] and an estimated time until the volume is full.
///
/// A sampler resumes only with proof that the newest segment is the file its previous samples scanned and that it
/// still holds what they saw: it keeps that file open between samples, so its inode can't be reused by another file
/// (ext4 reuses inode numbers at once), and it re-reads the last record they passed. Otherwise (a journal replaced at
/// the same path, a segment repaired by recovery or restored from a copy) it scans afresh and drops its rates. If the
/// checkpoint marker moved to a position the previous scans passed, the slot there is checked against the marker
/// before resuming. Nothing else is carried from one sample to the next.
///
/// Holds one file open between samples (the newest segment at the previous sample; if retention removes it meanwhile,
/// its space is freed at the next sample) and starts no thread.
#[derive(Debug, Clone)]
pub struct Sampler {
    dir: PathBuf,
    readers_dir: PathBuf,
    previous: Option<Previous>,
}

#[derive(Debug, Clone)]
struct Previous {
    at: Instant,
    head: Head,
    /// The head's segment file.
    head_seg: SegId,
    /// Kept open so that `head_seg`'s inode can't be reused while the sampler relies on it: a file with that identity
    /// is the same file.
    _pinned: std::sync::Arc<File>,
    /// Where the scans of `head_seg` that led to `head` started (they validated everything from there).
    origin: u64,
    /// The last record those scans passed (its offset and header), re-read before resuming.
    last_record: Option<(u64, RecordHeader)>,
    /// The checkpoint marker's slot at that sample.
    marker: Option<CheckpointSlot>,
}

/// A segment file as listed: its base, size and file identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SegFile {
    base: u64,
    size: u64,
    file: Identity,
}

/// A segment file's full identity: name, file, and header. Inode numbers are reused (ext4 reuses them at once), so
/// the header's creation time is part of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SegId {
    base: u64,
    file: Identity,
    created_unix_ns: u64,
    size: u64,
}

/// A segment opened during one sample.
#[derive(Debug)]
struct Opened {
    id: SegId,
    file: File,
    path: PathBuf,
}

/// Segments opened during one sample, so that each is opened at most once.
#[derive(Debug, Default)]
struct OpenSegments {
    opened: Vec<Opened>,
}

impl OpenSegments {
    /// The segment `base`, opened and its header validated; errors as for [`open_segment`].
    fn get(&mut self, dir: &Path, base: u64) -> Result<&Opened> {
        if let Some(i) = self.opened.iter().position(|o| o.id.base == base) {
            return Ok(&self.opened[i]);
        }
        let opened = open_segment(dir, base)?;
        self.opened.push(opened);
        Ok(self.opened.last().expect("just pushed"))
    }

    /// Takes the open segment `base` out (to keep it open after the sample).
    fn take(&mut self, base: u64) -> Option<File> {
        let i = self.opened.iter().position(|o| o.id.base == base)?;
        Some(self.opened.swap_remove(i).file)
    }
}

impl Sampler {
    /// A sampler for the journal in `dir`, reading beacons from `<dir>/readers`. Does no I/O.
    pub fn new(dir: impl AsRef<Path>) -> Sampler {
        let dir = dir.as_ref().to_path_buf();
        Sampler {
            readers_dir: dir.join(READERS_DIR),
            dir,
            previous: None,
        }
    }

    /// Reads beacons from `readers_dir` instead of `<dir>/readers` (for readers that can't write to the journal
    /// directory; see [`ReaderBeacon::create_in`]).
    pub fn readers_dir(mut self, readers_dir: impl AsRef<Path>) -> Sampler {
        self.readers_dir = readers_dir.as_ref().to_path_buf();
        self
    }

    /// Takes a sample. Costs as for [`status`], except that while the newest segment is the file the previous sample
    /// scanned, it is scanned only from the previous sample's head (or from the checkpoint, if that is further on),
    /// so a steady-state sample reads only the bytes appended since the previous one, plus one 4 KiB `pread` to
    /// re-read the last record the previous scans passed (and another of the slot at the checkpoint marker, if it
    /// moved to a position those scans passed), and the record right before the marker's position, as [`status`] does; and that when the head moved to another segment, the previous head's
    /// segment header is read (one `open`, two `fstat`s, `pread`, `fstat`) to check that the history continued before
    /// rates are reported.
    ///
    /// # Errors
    ///
    /// As for [`status`]. A failed sample leaves the sampler's state as it was.
    pub fn sample(&mut self) -> Result<Status> {
        let mut attempt = 1;
        loop {
            match self.sample_once() {
                Err(Error::Io { source, .. })
                    if source.kind() == io::ErrorKind::NotFound && attempt < SAMPLE_ATTEMPTS =>
                {
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    fn sample_once(&mut self) -> Result<Status> {
        let at = Instant::now();
        let mut leftovers = Vec::new();
        let (mut files, mut marker) = list_with_marker(&self.dir, &mut leftovers)?;
        let mut open = OpenSegments::default();

        // The head: scan the newest segment; if the writer rolled meanwhile, list again (and read the marker again,
        // so that the two agree) and take the new newest.
        let mut rolls = 0;
        let (head, head_seg, origin, start, scan_first, last_record, proven) = loop {
            let slot = marker.as_ref().and_then(|m| m.slot);
            let newest = *files
                .last()
                .expect("list_with_marker never returns an empty listing");
            let seg = open.get(&self.dir, newest.base)?;
            let id = seg.id;
            let size = id.size;
            let marker_here = slot
                .filter(|c| c.segment_base_seq == id.base && valid_offset(c.offset, size))
                .map(|c| Position::new(c.next_seq, c.segment_base_seq, c.offset));
            let fresh = match marker_here {
                Some(m) => (m, m.offset),
                None => (
                    Position::new(id.base, id.base, format::SEGMENT_HEADER_LEN as u64),
                    format::SEGMENT_HEADER_LEN as u64,
                ),
            };
            // The previous samples' file, unchanged where they stopped: same identity (its inode is pinned, so not
            // reused) and the same last record.
            let proven = self
                .previous
                .as_ref()
                .filter(|p| p.head_seg == id && valid_offset(p.head.position.offset, size))
                .filter(|p| p.last_record.is_none_or(|(at, h)| record_is(seg, at, h)));
            let resumed = proven.filter(|p| match marker_here {
                // The marker is past the previous head: start there (less to read, and validated from there on).
                Some(m) if m.offset > p.head.position.offset => false,
                // The marker moved to a position the previous scans passed: it must be a record boundary with the
                // marker's sequence number, as a scan from it would check.
                Some(m) if p.marker != slot && m.offset >= p.origin => {
                    if m.offset == p.head.position.offset {
                        m.next_seq == p.head.position.next_seq
                    } else {
                        matches!(slot_at(seg, m), Some(Ok(Slot::Record { .. })))
                    }
                }
                _ => true,
            });
            let (start, origin, carried) = match resumed {
                Some(p) => (p.head.position, p.origin, Some(p)),
                None => (fresh.0, fresh.1, None),
            };
            let scan = scan_from(&seg.file, &seg.path, size, start)?;
            if scan.end == HeadEnd::Sealed {
                let successor = journal::segment_path(&self.dir, scan.end_position.next_seq);
                if std::fs::symlink_metadata(&successor).is_ok() {
                    rolls += 1;
                    if rolls >= ROLL_ATTEMPTS {
                        return Err(Error::io(
                            "sample journal head",
                            &self.dir,
                            io::Error::other(format!(
                                "the writer rolled {ROLL_ATTEMPTS} times during one sample"
                            )),
                        ));
                    }
                    (files, marker) = list_with_marker(&self.dir, &mut leftovers)?;
                    continue;
                }
            }
            // The record to re-read next time: the last one this scan passed, else the proven one from before (still
            // in this file, before the head).
            let last_record = match (scan.last, scan.last_at) {
                (Some(h), Some(at)) => Some((at, h)),
                _ => proven.and_then(|p| p.last_record),
            };
            let head = Head {
                position: scan.end_position,
                segment_size: size,
                end: scan.end,
                last_local_ts_ns: match scan.last {
                    Some(h) => Some(h.local_ts_ns).filter(|&ts| ts != 0),
                    None => carried.and_then(|p| p.head.last_local_ts_ns),
                },
                verified: false,
                chain_broken: false,
            };
            break (
                head,
                id,
                origin,
                start,
                scan.first,
                last_record,
                proven.is_some(),
            );
        };
        let mut head = head;
        let first = files[0];
        let slot = marker.as_ref().and_then(|m| m.slot);

        // Readers, observed before the clock is read, so that a beacon written during the sample is never in the
        // future.
        let (readers_dir_state, observed) = beacon::observe_all(&self.readers_dir);
        // After the marker and the beacons, so that a checkpoint or a beacon write landing during the sample is
        // never in the future.
        let now_ns = crate::clock::realtime_ns();

        // The checkpoint, checked as the writer checks it at open.
        let checkpoint = match (slot, marker.as_ref()) {
            (Some(c), Some(m)) => Some(checkpoint_status(
                &self.dir, &files, &mut open, c, m, now_ns,
            )?),
            _ => None,
        };
        let usable = checkpoint.filter(|c| c.problem.is_none());
        let covered_from = match (&checkpoint, usable) {
            (_, Some(c)) => Some(c.position()),
            (None, _) => Some(Position::new(
                first.base,
                first.base,
                format::SEGMENT_HEADER_LEN as u64,
            )),
            (Some(_), None) => None,
        };
        head.chain_broken = covered_from.is_some_and(|c| missing_segment(&files, c, head_seg.base));
        // A head at invalid bytes is a break the sample confirms (the writer refuses the journal): nothing up to it is
        // verified, and no exposure is summed up to it (round 3).
        let head_invalid = matches!(head.end, HeadEnd::Invalid(_));
        head.verified = !head.chain_broken
            && !head_invalid
            && covered_from.is_some_and(|c| {
                c.segment_base_seq == head_seg.base
                    && origin <= c.offset
                    && c.next_seq <= head.position.next_seq
            });

        // Segments summary.
        let oldest_created_unix_ns = open
            .get(&self.dir, first.base)
            .ok()
            .map(|o| o.id.created_unix_ns);
        let segments = Segments {
            count: files.len() as u64,
            first_seq: first.base,
            total_bytes: files.iter().fold(0u64, |sum, f| sum.saturating_add(f.size)),
            oldest_created_unix_ns,
            before_checkpoint: checkpoint.map_or(0, |c| {
                files
                    .iter()
                    .filter(|f| f.base < c.marker.segment_base_seq)
                    .count() as u64
            }),
        };

        // Exposure. Nothing is summed across a segment the listing proves missing. The oldest uncovered record's
        // timestamp is looked up afresh in every sample (one record read), never carried from an earlier one.
        let records = covered_from
            .filter(|_| !head.chain_broken && !head_invalid)
            .and_then(|c| head.position.next_seq.checked_sub(c.next_seq));
        let oldest_local_ts_ns = match (records, covered_from) {
            (Some(n), Some(c)) if n > 0 => {
                if start == c && scan_first.is_some() {
                    scan_first.map(|h| h.local_ts_ns).filter(|&ts| ts != 0)
                } else {
                    open.get(&self.dir, c.segment_base_seq)
                        .ok()
                        .and_then(|seg| record_at(&self.dir, seg, c))
                        .map(|h| h.local_ts_ns)
                        .filter(|&ts| ts != 0)
                }
            }
            _ => None,
        };
        let exposure = Exposure {
            records,
            bytes: match (records, covered_from) {
                (Some(0), _) => Some(0),
                (Some(_), Some(c)) => footprint(&files, c, head.position),
                _ => None,
            },
            estimated_age: match records {
                Some(0) => Some(Duration::ZERO),
                Some(_) => usable.and_then(|c| c.estimated_age),
                None => None,
            },
            oldest_local_ts_ns,
            verified: head.verified,
        };

        // Rates, if the history provably continued from the previous head.
        let rates = self.previous.as_ref().and_then(|p| {
            let interval = at.checked_duration_since(p.at)?;
            let secs = interval.as_secs_f64();
            if secs <= 0.0 || head.position.next_seq < p.head.position.next_seq {
                return None;
            }
            // The previous head's file is pinned, so a file with its identity is that file.
            let continued = if p.head_seg == head_seg {
                proven
            } else {
                p.head_seg.base < head_seg.base
                    && open
                        .get(&self.dir, p.head_seg.base)
                        .is_ok_and(|o| o.id == p.head_seg)
            };
            continued.then(|| Rates {
                interval,
                records_per_sec: (head.position.next_seq - p.head.position.next_seq) as f64 / secs,
                bytes_per_sec: footprint(&files, p.head.position, head.position)
                    .map(|b| b as f64 / secs),
            })
        });
        let storage = storage(&self.dir, &head, rates)?;
        let writer = writer_process(&self.dir);

        let mut status = Status {
            dir: self.dir.clone(),
            sampled_unix_ns: now_ns,
            head,
            segments,
            checkpoint,
            exposure,
            storage,
            rates,
            writer,
            readers_dir: self.readers_dir.clone(),
            readers_dir_state,
            readers: Vec::new(),
            removal_leftovers: leftovers,
            files,
        };
        let readers = observed
            .into_iter()
            .map(|o| reader_status(&status, &self.dir, &mut open, o, now_ns))
            .collect();
        status.readers = readers;

        let pinned = open
            .take(head_seg.base)
            .map(std::sync::Arc::new)
            .expect("the head's segment was opened by this sample");
        self.previous = Some(Previous {
            at,
            head,
            head_seg,
            _pinned: pinned,
            origin,
            last_record,
            marker: slot,
        });
        Ok(status)
    }
}

impl Reader {
    /// This reader's cursor: the sequence number of the next record it returns ([`Reader::position`]), the slot it
    /// reads next, whether a seek is still pending, whether it is durable-only, and its segment file. Pass it to
    /// [`Status::lag`].
    ///
    /// Cost: no system calls.
    pub fn cursor(&self) -> Cursor {
        self.cursor_parts().cursor()
    }
}

/// Lists the segment files and reads the checkpoint marker so that the two agree. If the marker names a segment the
/// listing lacks (the writer may have rolled and checkpointed into a new segment after the listing), the directory is
/// listed again and the marker read again, until the segment is listed or the marker didn't change across a listing:
/// retention never removes the checkpoint's segment, so a segment missing from a listing taken while the marker named
/// it is really missing. Up to [`MARKER_ATTEMPTS`] times; then an I/O error (the marker kept moving to segments that
/// weren't listed yet). Costs nothing extra when the marker's segment is listed.
///
/// An empty listing is [`Error::NotFound`], unless the marker names a segment: the writer refuses such a journal
/// (dangling checkpoint), so it is [`Error::Corrupt`].
///
/// `leftovers` gets the temporary names of interrupted removals the last listing showed ([`Status::removal_leftovers`]).
fn list_with_marker(
    dir: &Path,
    leftovers: &mut Vec<String>,
) -> Result<(Vec<SegFile>, Option<Marker>)> {
    let mut files = list_files_and(dir, leftovers)?;
    let mut marker = read_marker(dir)?;
    let named = |m: &Option<Marker>| m.as_ref().and_then(|m| m.slot);
    let mut attempts = 0;
    while let Some(c) =
        named(&marker).filter(|c| files.iter().all(|f| f.base != c.segment_base_seq))
    {
        attempts += 1;
        if attempts > MARKER_ATTEMPTS {
            return Err(Error::io(
                "sample journal",
                dir,
                io::Error::other(
                    "the checkpoint marker kept naming segments the listing didn't show yet",
                ),
            ));
        }
        files = list_files_and(dir, leftovers)?;
        let again = read_marker(dir)?;
        let unchanged = named(&again) == Some(c);
        marker = again;
        if unchanged {
            // Confirmed missing if still unlisted: the marker named it during the whole listing.
            break;
        }
    }
    if files.is_empty() {
        return Err(match named(&marker) {
            Some(c) => Error::corrupt(
                dir.join(journal::CHECKPOINT_FILE),
                0,
                CorruptReason::DanglingCheckpoint(c.segment_base_seq),
            ),
            None => Error::NotFound {
                dir: dir.to_path_buf(),
            },
        });
    }
    Ok((files, marker))
}

/// Lists the segment files with their sizes and identities (possibly none).
#[cfg(test)]
fn list_files(dir: &Path) -> Result<Vec<SegFile>> {
    list_files_and(dir, &mut Vec::new())
}

/// The segment files with their sizes and identities (possibly none), and the temporary names of interrupted removals
/// (ADR 0055) into `leftovers`, from the same listing.
fn list_files_and(dir: &Path, leftovers: &mut Vec<String>) -> Result<Vec<SegFile>> {
    leftovers.clear();
    let listed = journal::list_bases_and(dir, |name| {
        if crate::retention::is_leftover_name(name) {
            leftovers.push(name.to_owned());
        }
    })?;
    leftovers.sort();
    let mut files = Vec::with_capacity(listed.len());
    for segment in &listed {
        let base = &segment.base;
        let path = &journal::segment_path(dir, *base);
        let meta = std::fs::metadata(path).map_err(|e| Error::io("stat segment", path, e))?;
        files.push(SegFile {
            base: *base,
            size: meta.len(),
            file: (meta.dev(), meta.ino()),
        });
    }
    Ok(files)
}

/// Opens a file for reading without blocking (a FIFO or device can't hang the caller), optionally without following
/// a symlink, and requires a regular file.
fn open_plain(path: &Path, follow: bool) -> io::Result<File> {
    let mut flags = libc::O_NONBLOCK | libc::O_CLOEXEC;
    if !follow {
        flags |= libc::O_NOFOLLOW;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(file)
}

/// Opens segment `base` and validates its header (as the reader and the writer do).
fn open_segment(dir: &Path, base: u64) -> Result<Opened> {
    let path = journal::segment_path(dir, base);
    let file = open_plain(&path, true).map_err(|e| Error::io("open segment", &path, e))?;
    let header: SegmentHeader = journal::read_header(&file, &path, base)?;
    let file_id = journal::file_identity(&file, &path)?;
    Ok(Opened {
        id: SegId {
            base,
            file: file_id,
            created_unix_ns: header.created_unix_ns,
            size: header.segment_size,
        },
        file,
        path,
    })
}

/// Whether `offset` can hold a slot in a segment of `size` bytes.
pub(crate) fn valid_offset(offset: u64, size: u64) -> bool {
    offset % format::RECORD_ALIGN as u64 == 0
        && offset >= format::SEGMENT_HEADER_LEN as u64
        && offset <= size.saturating_sub(format::SEGMENT_TRAILER_LEN as u64)
}

/// Segment-file footprint from `from` to `to`: the rest of `from`'s segment, every segment in between, and `to`'s
/// offset. `None` if `to` is before `from`, or a segment involved isn't listed.
fn footprint(files: &[SegFile], from: Position, to: Position) -> Option<u64> {
    if from.segment_base_seq == to.segment_base_seq {
        return to.offset.checked_sub(from.offset);
    }
    if from.segment_base_seq > to.segment_base_seq {
        return None;
    }
    let i = files
        .binary_search_by_key(&from.segment_base_seq, |f| f.base)
        .ok()?;
    let j = files
        .binary_search_by_key(&to.segment_base_seq, |f| f.base)
        .ok()?;
    let rest = files[i].size.saturating_sub(from.offset);
    let between = files[i + 1..j]
        .iter()
        .fold(0u64, |sum, f| sum.saturating_add(f.size));
    Some(rest.saturating_add(between).saturating_add(to.offset))
}

/// Whether the listing alone proves a segment missing between `from` (the checkpoint's position, or the start of the
/// oldest segment) and the head's segment `head_base`, which [`Writer::open`](crate::Writer::open) refuses as a
/// broken chain: a segment can hold no more records than fit in its record area at the smallest frame (40 bytes), so
/// the next listed segment's base can't lie further on. A missing segment that this doesn't prove (most of them: the
/// sample doesn't read the segments in between) leaves the chain unverified instead ([`Head::verified`]).
fn missing_segment(files: &[SegFile], from: Position, head_base: u64) -> bool {
    let min_frame = format::align8(format::RECORD_HEADER_LEN as u64);
    let (Ok(i), Ok(j)) = (
        files.binary_search_by_key(&from.segment_base_seq, |f| f.base),
        files.binary_search_by_key(&head_base, |f| f.base),
    ) else {
        return false;
    };
    let (mut seq, mut offset) = (from.next_seq, from.offset);
    for k in i..j {
        let end = files[k]
            .size
            .saturating_sub(format::SEGMENT_TRAILER_LEN as u64);
        let most = seq.saturating_add(end.saturating_sub(offset) / min_frame);
        if files[k + 1].base > most {
            return true;
        }
        seq = files[k + 1].base;
        offset = format::SEGMENT_HEADER_LEN as u64;
    }
    false
}

struct ScanOut {
    end_position: Position,
    end: HeadEnd,
    first: Option<RecordHeader>,
    last: Option<RecordHeader>,
    /// Offset of `last`.
    last_at: Option<u64>,
}

/// Reads the slot at `offset` (expecting `seq`), re-reading up to [`VALIDATION_RETRIES`] times while it fails
/// validation.
fn read_slot_retrying(
    view: &PreadView<'_>,
    path: &Path,
    offset: u64,
    seq: u64,
) -> Result<std::result::Result<Slot, CorruptReason>> {
    let mut attempt = 0;
    loop {
        let slot = format::read_slot(view, offset, seq);
        if let Some(e) = view.take_error() {
            return Err(Error::io("read segment", path, e));
        }
        match slot {
            Err(_) if attempt < VALIDATION_RETRIES => {
                attempt += 1;
                view.invalidate();
            }
            other => return Ok(other),
        }
    }
}

/// Scans a segment forward from `start` to the first slot that holds no valid record.
fn scan_from(file: &File, path: &Path, size: u64, start: Position) -> Result<ScanOut> {
    let view = PreadView::new(file, size, journal::SCAN_WINDOW);
    let mut position = start;
    let mut first = None;
    let mut last = None;
    let mut last_at = None;
    let end = loop {
        match read_slot_retrying(&view, path, position.offset, position.next_seq)? {
            Ok(Slot::Empty) => break HeadEnd::Open,
            Ok(Slot::Seal) => break HeadEnd::Sealed,
            Err(reason) => break HeadEnd::Invalid(reason),
            Ok(Slot::Record {
                header,
                next_offset,
                ..
            }) => {
                let Some(next_seq) = position.next_seq.checked_add(1) else {
                    break HeadEnd::Invalid(CorruptReason::SequenceExhausted);
                };
                first.get_or_insert(header);
                last = Some(header);
                last_at = Some(position.offset);
                position.offset = next_offset;
                position.next_seq = next_seq;
            }
        }
    };
    Ok(ScanOut {
        end_position: position,
        end,
        first,
        last,
        last_at,
    })
}

/// The slot at `position` in the open segment `seg`, validated for `position.next_seq` (re-read while it fails, as a
/// scan does); `None` if it can't be read. One `pread` of 4 KiB (more for a larger record).
fn slot_at(seg: &Opened, position: Position) -> Option<std::result::Result<Slot, CorruptReason>> {
    let view = PreadView::new(&seg.file, seg.id.size, RECORD_WINDOW);
    read_slot_retrying(&view, &seg.path, position.offset, position.next_seq).ok()
}

/// Whether the slot at `offset` in `seg` still holds the record `header` (same sequence number, lengths, timestamps,
/// schema and checksum).
fn record_is(seg: &Opened, offset: u64, header: RecordHeader) -> bool {
    matches!(
        slot_at(seg, Position::new(header.seq, seg.id.base, offset)),
        Some(Ok(Slot::Record { header: h, .. })) if h == header
    )
}

/// The header of the record at `position` in the open segment `seg`, following one seal into the next segment;
/// `None` if there is no valid record there (yet) or it can't be read. Informational, so errors are not reported.
fn record_at(dir: &Path, seg: &Opened, position: Position) -> Option<RecordHeader> {
    let view = PreadView::new(&seg.file, seg.id.size, RECORD_WINDOW);
    match read_slot_retrying(&view, &seg.path, position.offset, position.next_seq).ok()? {
        Ok(Slot::Record { header, .. }) => Some(header),
        Ok(Slot::Seal) => {
            let next = open_segment(dir, position.next_seq).ok()?;
            let view = PreadView::new(&next.file, next.id.size, RECORD_WINDOW);
            let at = format::SEGMENT_HEADER_LEN as u64;
            match read_slot_retrying(&view, &next.path, at, position.next_seq).ok()? {
                Ok(Slot::Record { header, .. }) => Some(header),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The checkpoint marker as read.
#[derive(Debug, Clone, Copy)]
struct Marker {
    slot: Option<CheckpointSlot>,
    len: u64,
    modified_unix_ns: Option<u64>,
    /// The metadata kept changing over every attempt.
    changing: bool,
    /// The current slot's generation is 2 or more and the other slot doesn't hold the generation before it.
    older_slot_lost: bool,
}

/// What must not change while the marker is read.
fn marker_stamp(meta: &std::fs::Metadata) -> (u64, u64, i64, i64, i64, i64) {
    (
        meta.ino(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}

/// Reads the checkpoint marker's two slots and its modification time consistently: `fstat`, the slots, `fstat`
/// again, retried while the metadata changes.
fn read_marker(dir: &Path) -> Result<Option<Marker>> {
    let path = dir.join(journal::CHECKPOINT_FILE);
    let file = match open_plain(&path, true) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io("open checkpoint", &path, e)),
    };
    let stat = |file: &File| {
        file.metadata()
            .map_err(|e| Error::io("stat checkpoint", &path, e))
    };
    let mut last = None;
    for _ in 0..MARKER_ATTEMPTS {
        let before = stat(&file)?;
        let len = before.len();
        let mut slots = [None, None];
        let mut short = false;
        for (slot, at) in slots.iter_mut().zip(format::CHECKPOINT_SLOT_OFFSETS) {
            let end = (at + format::CHECKPOINT_SLOT_LEN) as u64;
            if end > len.min(format::CHECKPOINT_FILE_LEN as u64) {
                continue;
            }
            let mut buf = [0u8; format::CHECKPOINT_SLOT_LEN];
            match file.read_exact_at(&mut buf, at as u64) {
                Ok(()) => *slot = format::decode_checkpoint_slot(&buf),
                // Cut short under the read: the metadata check below retries.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => short = true,
                Err(e) => return Err(Error::io("read checkpoint", &path, e)),
            }
        }
        let after = stat(&file)?;
        // On a tie the slot at 65,536 wins (docs/format.md), as in `format::decode_checkpoint`.
        let slot = match slots {
            [Some(a), Some(b)] => Some(if a.generation > b.generation { a } else { b }),
            [a, b] => a.or(b),
        };
        let marker = Marker {
            slot,
            older_slot_lost: slot.is_some_and(|current| {
                current.generation >= 2
                    && !slots
                        .iter()
                        .flatten()
                        .any(|s| s.generation == current.generation - 1)
            }),
            len,
            modified_unix_ns: before
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)),
            changing: false,
        };
        if !short && marker_stamp(&before) == marker_stamp(&after) {
            return Ok(Some(marker));
        }
        last = Some(marker);
    }
    Ok(last.map(|m| Marker {
        changing: true,
        ..m
    }))
}

/// Validates the marker `c` as [`Writer::open`](crate::Writer::open) does, in the writer's order (its segment, its
/// header, its offset, the record before its offset: the writer's own check, [`journal::marker_reached`]), and estimates
/// its age.
fn checkpoint_status(
    dir: &Path,
    files: &[SegFile],
    open: &mut OpenSegments,
    c: CheckpointSlot,
    marker: &Marker,
    now_ns: u64,
) -> Result<CheckpointStatus> {
    let mut created = None;
    // `list_with_marker` confirmed that a segment missing from the listing is missing.
    let problem = if files.iter().all(|f| f.base != c.segment_base_seq) {
        Some(CheckpointProblem::MissingSegment)
    } else {
        match open.get(dir, c.segment_base_seq) {
            Ok(seg) => {
                created = Some(seg.id.created_unix_ns);
                if !valid_offset(c.offset, seg.id.size) {
                    Some(CheckpointProblem::InvalidOffset)
                } else if !journal::marker_reached(&seg.file, &seg.path, seg.id.size, &c)? {
                    // The writer's own check (ADR 0056), over the same bytes: the record right before the offset.
                    Some(CheckpointProblem::RecordsMissing)
                } else {
                    None
                }
            }
            Err(Error::Corrupt { .. } | Error::UnsupportedVersion { .. }) => {
                Some(CheckpointProblem::InvalidSegmentHeader)
            }
            // Listed, then gone: retention removed it after the marker moved on. Not a problem of this marker; the
            // error makes the sampler try again with a fresh listing and marker.
            Err(e) => return Err(e),
        }
    };
    let age_unknown = if marker.changing {
        Some(AgeUnknown::Changing)
    } else if marker.len != format::CHECKPOINT_FILE_LEN as u64 {
        Some(AgeUnknown::MarkerResized)
    } else if marker.older_slot_lost {
        Some(AgeUnknown::OlderSlotLost)
    } else {
        match marker.modified_unix_ns {
            None => Some(AgeUnknown::NoModificationTime),
            Some(m) if m > now_ns => Some(AgeUnknown::InFuture),
            Some(m) if created.is_some_and(|c| m.saturating_add(MTIME_SLACK_NS) < c) => {
                Some(AgeUnknown::BeforeSegment)
            }
            Some(_) => None,
        }
    };
    let estimated_age = match (age_unknown, marker.modified_unix_ns) {
        (None, Some(m)) => Some(Duration::from_nanos(now_ns - m)),
        _ => None,
    };
    Ok(CheckpointStatus {
        marker: c,
        problem,
        modified_unix_ns: marker.modified_unix_ns,
        estimated_age,
        age_unknown,
    })
}

fn storage(dir: &Path, head: &Head, rates: Option<Rates>) -> Result<Storage> {
    let vfs = rustix::fs::statvfs(dir)
        .map_err(|e| Error::io("statvfs journal directory", dir, e.into()))?;
    let available_bytes = vfs.f_bavail.saturating_mul(vfs.f_frsize);
    let total_bytes = vfs.f_blocks.saturating_mul(vfs.f_frsize);
    let spare = std::fs::symlink_metadata(dir.join(journal::SPARE_FILE))
        .ok()
        .filter(|m| m.is_file() && format::validate_segment_size(m.len()).is_ok())
        // A sparse file of the right length holds no space: count it only if its blocks are allocated.
        .map(|m| (m.len(), m.blocks().saturating_mul(512) >= m.len()));
    Ok(estimate_storage(
        available_bytes,
        total_bytes,
        spare,
        head,
        rates,
    ))
}

/// The storage estimates from the observed figures: `spare` is the prepared spare's length and whether its blocks are
/// allocated. Separate from [`storage`] so that tests can give it fixed figures (free space changes under them).
fn estimate_storage(
    available_bytes: u64,
    total_bytes: u64,
    spare: Option<(u64, bool)>,
    head: &Head,
    rates: Option<Rates>,
) -> Storage {
    let next_segment_size = spare.map_or(head.segment_size, |(len, _)| len).max(1);
    let spare_segment = spare.is_some_and(|(_, allocated)| allocated);
    let estimated_rolls_left = available_bytes / next_segment_size;
    let estimated_time_to_full = rates
        .and_then(|r| r.bytes_per_sec)
        .filter(|&rate| rate > 0.0)
        .and_then(|rate| {
            let left_in_active = match head.end {
                HeadEnd::Sealed => 0,
                _ => head.segment_size.saturating_sub(head.position.offset),
            };
            let bytes = left_in_active
                .saturating_add(estimated_rolls_left.saturating_mul(next_segment_size));
            Duration::try_from_secs_f64(bytes as f64 / rate).ok()
        });
    Storage {
        available_bytes,
        total_bytes,
        next_segment_size,
        spare_segment,
        estimated_rolls_left,
        estimated_time_to_full,
    }
}

fn writer_process(dir: &Path) -> WriterProcess {
    use std::io::Read;

    let pid = open_plain(&dir.join(journal::LOCK_FILE), false)
        .ok()
        .and_then(|f| {
            let mut text = String::new();
            f.take(32).read_to_string(&mut text).ok()?;
            text.trim().parse::<u32>().ok()
        });
    let alive = pid
        .and_then(|p| i32::try_from(p).ok())
        .and_then(rustix::process::Pid::from_raw)
        .and_then(|p| match rustix::process::test_kill_process(p) {
            Ok(()) => Some(true),
            Err(e) if e == rustix::io::Errno::PERM => Some(true),
            Err(e) if e == rustix::io::Errno::SRCH => Some(false),
            Err(_) => None,
        });
    WriterProcess { pid, alive }
}

/// A reader's status from its observed beacon: checks that the beacon's segment is this journal's file before
/// computing lag.
fn reader_status(
    status: &Status,
    dir: &Path,
    open: &mut OpenSegments,
    observed: beacon::Observed,
    now_ns: u64,
) -> ReaderStatus {
    let mut out = ReaderStatus {
        name: observed.name,
        path: observed.path,
        state: observed.state,
        report: None,
        update_age: None,
        lag: None,
        lag_unknown: None,
        oldest_unread_local_ts_ns: None,
        unread_age: None,
    };
    let Some(payload) = observed.payload else {
        return out;
    };
    let report = payload.report;
    out.report = Some(report);
    out.update_age = now_ns
        .checked_sub(report.updated_unix_ns)
        .map(Duration::from_nanos);
    let base = report.position.segment_base_seq;
    let identity = match status.listed(base) {
        Err(reason) => Err(reason),
        Ok(f) if f.file != payload.segment_file || f.size != payload.segment_size => {
            Err(LagUnknown::OtherHistory)
        }
        Ok(_) => match open.get(dir, base) {
            Ok(seg)
                if seg.id.file == payload.segment_file
                    && seg.id.size == payload.segment_size
                    && seg.id.created_unix_ns == payload.segment_created_unix_ns =>
            {
                Ok(())
            }
            Ok(_) => Err(LagUnknown::OtherHistory),
            Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                Err(LagUnknown::SegmentGone)
            }
            Err(_) => Err(LagUnknown::SegmentUnreadable),
        },
    };
    let lag = identity.and_then(|()| {
        status.lag_from(
            report.next_seq,
            report.position,
            report.seek_pending,
            report.durable_only,
        )
    });
    match lag {
        Err(reason) => out.lag_unknown = Some(reason),
        Ok(lag) => {
            out.lag = Some(lag);
            if lag.records == 0 {
                out.unread_age = Some(Duration::ZERO);
            } else if !report.seek_pending {
                out.oldest_unread_local_ts_ns = open
                    .get(dir, base)
                    .ok()
                    .and_then(|seg| record_at(dir, seg, report.position))
                    .map(|h| h.local_ts_ns)
                    .filter(|&ts| ts != 0);
                // A timestamp later than the observer's clock gives no age (unknown), not zero.
                out.unread_age = out
                    .oldest_unread_local_ts_ns
                    .and_then(|ts| now_ns.checked_sub(ts))
                    .map(Duration::from_nanos);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
