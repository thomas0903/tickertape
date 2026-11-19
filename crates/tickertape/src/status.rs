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
//! Exporting to a metrics system takes a few lines of application code; see `docs/operations.md` → Monitoring for
//! Prometheus and OpenTelemetry examples. The CLI's `tickertape status` prints the same values as text, JSON or
//! Prometheus text format.
//!
//! ```no_run
//! # fn main() -> tickertape::Result<()> {
//! let status = tickertape::status::status("/tmp/journal")?;
//! println!(
//!     "next seq {}, {} records not yet durable, {} more segments fit on the volume",
//!     status.head.position.next_seq, status.exposure.records, status.storage.rolls_left,
//! );
//! # Ok(())
//! # }
//! ```

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::error::{CorruptReason, Error, Result};
use crate::format::{self, CheckpointSlot, RecordHeader, Slot};
use crate::journal::{self, Identity, PreadView};
use crate::reader::Reader;

/// Name of the default beacon directory inside a journal directory (`<dir>/readers`).
pub const READERS_DIR: &str = "readers";
/// Default minimum time between two beacon writes (see [`ReaderBeacon::set_interval`]).
pub const DEFAULT_BEACON_INTERVAL: Duration = Duration::from_secs(1);

/// Re-reads of a slot that fails validation before it counts as invalid (the copy may predate the commit).
const VALIDATION_RETRIES: u32 = 3;
/// `pread` size when reading a single record.
const RECORD_WINDOW: usize = 4096;
/// Attempts at a sample when a file disappears under it (retention or recovery moved it).
const SAMPLE_ATTEMPTS: u32 = 3;
const BEACON_LEN: usize = 64;
const BEACON_MAGIC: [u8; 8] = *b"TTREADER";
const BEACON_VERSION: u32 = 1;
const BEACON_NAME_MAX: usize = 64;

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

/// How the newest segment ends, as seen by the sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HeadEnd {
    /// At a slot with no record yet: the normal state.
    Open,
    /// At a seal whose successor doesn't exist yet: a writer died during a roll, or is between its steps. The next
    /// writer creates the successor (`docs/format.md` → Recovery).
    Sealed,
    /// At bytes that still failed validation after re-reading. Not possible after a writer process crash; seen after
    /// a power loss or storage corruption, until the next writer repairs it.
    Invalid(CorruptReason),
}

/// The writer's head: where the next record will go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Head {
    /// Where the next record will be written.
    pub position: Position,
    /// Size of the newest segment, in bytes.
    pub segment_size: u64,
    /// What the sample found at `position`.
    pub end: HeadEnd,
    /// `local_ts_ns` of the last record in the newest segment, if it has one and the timestamp is set (non-zero).
    /// Caller-supplied data, in the producer's clock.
    pub last_local_ts_ns: Option<u64>,
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

/// The current checkpoint marker and its age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckpointStatus {
    /// The current slot of the marker (`docs/format.md` → Checkpoint marker).
    pub marker: CheckpointSlot,
    /// When the marker was last written: its file modification time, in nanoseconds since the Unix epoch.
    /// `None` if the filesystem doesn't report it.
    pub written_unix_ns: Option<u64>,
    /// Time since `written_unix_ns`, by the observer's wall clock. Zero if the modification time is in the future.
    pub age: Option<Duration>,
    /// True if the modification time is later than the observer's clock (the clock was stepped back, or the file was
    /// modified by something else): `age` is then too small.
    pub clock_skew: bool,
}

/// What a power loss now could remove: everything after the checkpoint marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Exposure {
    /// Committed records not covered by the checkpoint marker (all records if there is no marker).
    pub records: u64,
    /// Segment-file bytes between the marker's position and the head (see [`Lag::bytes`]). `None` if the marker's
    /// segment is missing.
    pub bytes: Option<u64>,
    /// How long these records have been exposed, at most: zero if there are none, the checkpoint's age otherwise
    /// (every uncovered record was appended after it). `None` if records are uncovered and there is no marker, or its
    /// age is unknown.
    pub age: Option<Duration>,
    /// `local_ts_ns` of the oldest uncovered record, if there is one and its timestamp is set. Producer's clock.
    pub oldest_local_ts_ns: Option<u64>,
}

/// Space on the journal's filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Storage {
    /// Bytes available to unprivileged users (`statvfs`: `f_bavail * f_frsize`).
    pub available_bytes: u64,
    /// Size of the filesystem (`f_blocks * f_frsize`).
    pub total_bytes: u64,
    /// Size of the newest segment, taken as the size of the segments still to come.
    pub segment_size: u64,
    /// True if a prepared next segment (`next-segment.tmp`, ADR 0022) already holds the space for the next roll.
    pub spare_segment: bool,
    /// How many more segment rolls can succeed if nothing else uses the volume: `available_bytes / segment_size`, plus
    /// one with a prepared spare. Writes into the active segment need no new space (crash contract C10, except on
    /// btrfs); the roll after these fails with a storage-full error (C9).
    pub rolls_left: u64,
    /// Estimated time until the writer needs roll `rolls_left + 1`, at the append rate measured by a [`Sampler`]
    /// between its last two samples. `None` without a rate (the first sample) or when nothing was appended.
    pub time_to_full: Option<Duration>,
}

/// Append rates measured between two samples of a [`Sampler`].
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct Rates {
    /// Time between the two samples (monotonic clock).
    pub interval: Duration,
    /// Records appended per second.
    pub records_per_sec: f64,
    /// Segment-file bytes the head advanced per second. `None` if the earlier head's segment is gone.
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

/// Whether a beacon's owner is still running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BeaconState {
    /// The owning process holds the beacon's lock (or locks are unsupported there, so it can't be told apart).
    Live,
    /// Nobody holds the lock: the owner died without removing the file. Its position is history, not lag.
    Stale,
    /// The file isn't a valid beacon (wrong size, magic, version or checksum, even after re-reading).
    Invalid,
}

/// Lag of a reader behind the head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Lag {
    /// Records committed but not yet returned by the reader.
    pub records: u64,
    /// Segment-file bytes between the reader's cursor and the head, counting whole files in between (headers,
    /// trailers and unused tails included), so at most a record's size more per segment boundary than the unread
    /// record bytes. `None` if the reader's segment is no longer in the journal (retention removed it).
    pub bytes: Option<u64>,
}

/// A reader in another process, from its [`ReaderBeacon`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReaderStatus {
    /// The beacon's name (its file name).
    pub name: String,
    /// The beacon file.
    pub path: PathBuf,
    /// Whether its owner is running.
    pub state: BeaconState,
    /// The owner's PID, as it recorded it (informational).
    pub pid: Option<u32>,
    /// The reader's cursor at its last beacon write.
    pub position: Option<Position>,
    /// Wall-clock time of the last beacon write, nanoseconds since the Unix epoch.
    pub updated_unix_ns: Option<u64>,
    /// Time since that write, by the observer's wall clock (zero if in the future).
    pub update_age: Option<Duration>,
    /// Lag behind the head, as of the last beacon write.
    pub lag: Option<Lag>,
    /// `local_ts_ns` of the first record the reader has not returned, if it is committed and its timestamp is set.
    pub oldest_unread_local_ts_ns: Option<u64>,
    /// Observer's wall clock minus `oldest_unread_local_ts_ns`, zero if the reader has caught up. Meaningful when the
    /// producer stamps `local_ts_ns` from this host's wall clock (for example with [`crate::clock::realtime_ns`]).
    pub unread_age: Option<Duration>,
}

/// One sample of a journal's operational state. See [`status`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Status {
    /// The journal directory.
    pub dir: PathBuf,
    /// The observer's wall clock when the sample started, nanoseconds since the Unix epoch.
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
    /// Readers with a beacon in the beacon directory, sorted by name.
    pub readers: Vec<ReaderStatus>,
    /// `(base_seq, file size)` of every segment, for [`Status::lag`].
    sizes: Vec<(u64, u64)>,
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
    /// `None` if this writer hasn't checkpointed yet; [`status`] reports the marker's age from its modification time.
    pub since_checkpoint: Option<Duration>,
    /// Records appended but not covered by the checkpoint marker (with no marker, every record since sequence 0).
    pub uncheckpointed_records: u64,
    /// Bytes the next checkpoint has to sync: records and seals written since the last checkpoint (or, after open,
    /// since the marker), in every segment touched.
    pub uncheckpointed_bytes: u64,
    /// True after a failed checkpoint: later checkpoints fail with [`Error::DurabilityLost`].
    pub poisoned: bool,
    /// Retention runs that failed since open. The last error is [`Writer::last_retention_error`](crate::Writer::last_retention_error).
    pub retention_failures: u64,
}

/// Health thresholds for [`Status::breaches`]. All are off by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Thresholds {
    /// Breached if records are uncovered and the checkpoint is older than this, or there is no checkpoint. An idle
    /// journal whose records are all covered is healthy however old its checkpoint is.
    pub max_checkpoint_age: Option<Duration>,
    /// Breached if fewer bytes than this are available on the volume.
    pub min_headroom_bytes: Option<u64>,
    /// Breached if fewer than this many more rolls fit on the volume.
    pub min_headroom_segments: Option<u64>,
    /// Breached if a live reader beacon lags by more than this many records.
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
    /// The head is at invalid bytes ([`HeadEnd::Invalid`]). Always checked.
    HeadInvalid,
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
            Breach::CheckpointAge => "checkpoint_age",
            Breach::HeadroomBytes => "headroom_bytes",
            Breach::HeadroomSegments => "headroom_segments",
            Breach::ReaderLag { .. } => "reader_lag",
        }
    }
}

impl Status {
    /// Lag of a reader in this process, whose cursor is `cursor` ([`Reader::cursor`]), behind this sample's head.
    ///
    /// Cost: no system calls; a binary search over the segment list.
    pub fn lag(&self, cursor: Position) -> Lag {
        Lag {
            records: self.head.position.next_seq.saturating_sub(cursor.next_seq),
            bytes: distance(&self.sizes, cursor, self.head.position),
        }
    }

    /// The health checks this sample fails, given `thresholds`. [`Breach::HeadInvalid`] is always checked.
    ///
    /// Cost: no system calls.
    pub fn breaches(&self, thresholds: &Thresholds) -> Vec<Breach> {
        let mut out = Vec::new();
        if matches!(self.head.end, HeadEnd::Invalid(_)) {
            out.push(Breach::HeadInvalid);
        }
        if let Some(max) = thresholds.max_checkpoint_age {
            if self.exposure.records > 0 && self.exposure.age.is_none_or(|age| age > max) {
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
            .is_some_and(|min| self.storage.rolls_left < min)
        {
            out.push(Breach::HeadroomSegments);
        }
        if let Some(max) = thresholds.max_reader_lag {
            for reader in &self.readers {
                if reader.state == BeaconState::Live && reader.lag.is_some_and(|l| l.records > max)
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
/// it is taken (retention or recovery moved it), the sample is retried, up to 3 attempts.
///
/// **Cost** (one call): one `readdir` of the journal directory and one `stat` per segment file; for the checkpoint
/// marker one `open`, `fstat` and two 64-byte `pread`s; `open`, `fstat` and a header `pread` of the oldest and the
/// newest segment, and a `pread` scan (1 MiB windows) of the newest segment **from the checkpoint position if the
/// marker points into it, otherwise from its start**, so at most one segment; one `statvfs`; reading `writer.lock`
/// and one `kill(pid, 0)`; a `readdir` of the beacon directory and, per beacon, `open`, `flock` and a 64-byte
/// `pread`. Lagging beacons, and an exposure whose oldest record is in an older segment, cost one more segment
/// `open` and a 4 KiB `pread` each. Use a [`Sampler`] to sample repeatedly: it scans only what was appended since its
/// previous sample.
///
/// # Errors
///
/// [`Error::NotFound`] if `dir` has no segments; [`Error::Corrupt`] or [`Error::UnsupportedVersion`] if the newest
/// segment's header is invalid; [`Error::Io`].
pub fn status(dir: impl AsRef<Path>) -> Result<Status> {
    Sampler::new(dir).sample()
}

/// Samples a journal repeatedly, more cheaply than [`status`]: each sample resumes the head scan where the previous
/// one stopped, and successive samples give append [`Rates`] and an estimated time until the volume is full.
///
/// Holds no file open between samples and starts no thread.
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
    identity: Identity,
    /// The checkpoint generation and the oldest uncovered record's timestamp found for it.
    exposure_ts: Option<(u64, u64, Option<u64>)>,
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

    /// Takes a sample. Costs as for [`status`], except that the newest segment is scanned only from the previous
    /// sample's head while that segment is unchanged (same file, same base), so a steady-state sample reads only the
    /// bytes appended since the previous one.
    ///
    /// # Errors
    ///
    /// As for [`status`].
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
        let now_ns = crate::clock::realtime_ns();

        // Segments and their sizes.
        let listed = journal::list_segments(&self.dir)?;
        let (Some((first_base, first_path)), Some((newest_base, newest_path))) =
            (listed.first(), listed.last())
        else {
            return Err(Error::NotFound {
                dir: self.dir.clone(),
            });
        };
        let mut sizes = Vec::with_capacity(listed.len());
        for (base, path) in &listed {
            let meta = fs::metadata(path).map_err(|e| Error::io("stat segment", path, e))?;
            sizes.push((*base, meta.len()));
        }

        let marker = read_marker(&self.dir)?;

        // The head.
        let file =
            File::open(newest_path).map_err(|e| Error::io("open segment", newest_path, e))?;
        let header = journal::read_header(&file, newest_path, *newest_base)?;
        let identity = journal::file_identity(&file, newest_path)?;
        let size = header.segment_size;
        let start_of_segment = Position::new(
            *newest_base,
            *newest_base,
            format::SEGMENT_HEADER_LEN as u64,
        );
        let checkpoint_here = marker
            .map(|(slot, _)| slot)
            .filter(|c| c.segment_base_seq == *newest_base && valid_offset(c.offset, size))
            .map(|c| Position::new(c.next_seq, c.segment_base_seq, c.offset));
        let resumed = self
            .previous
            .as_ref()
            .filter(|p| {
                p.identity == identity
                    && p.head.position.segment_base_seq == *newest_base
                    && valid_offset(p.head.position.offset, size)
            })
            .map(|p| (p.head.position, p.head.last_local_ts_ns));
        let (start, carried_ts) = match (resumed, checkpoint_here) {
            (Some((position, ts)), _) => (position, ts),
            (None, Some(position)) => (position, None),
            (None, None) => (start_of_segment, None),
        };
        let mut scan = scan_from(&file, newest_path, size, start)?;
        drop(file);
        let first_record = scan.first;
        let mut last_record = scan.last;
        let (mut head_size, mut identity) = (size, identity);
        // A roll links the successor before it seals: if the listing predates the link but the scan saw the seal,
        // follow the seal as a reader would (bounded, in case rolls keep happening).
        for _ in 0..4 {
            if scan.end != HeadEnd::Sealed {
                break;
            }
            let base = scan.end_position.next_seq;
            let path = journal::segment_path(&self.dir, base);
            let file = match File::open(&path) {
                Ok(file) => file,
                Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                Err(e) => return Err(Error::io("open segment", &path, e)),
            };
            let header = journal::read_header(&file, &path, base)?;
            identity = journal::file_identity(&file, &path)?;
            head_size = header.segment_size;
            if sizes.last().is_some_and(|(b, _)| *b < base) {
                sizes.push((base, head_size));
            }
            scan = scan_from(
                &file,
                &path,
                head_size,
                Position::new(base, base, format::SEGMENT_HEADER_LEN as u64),
            )?;
            last_record = scan.last.or(last_record);
        }
        let head = Head {
            position: scan.end_position,
            segment_size: head_size,
            end: scan.end,
            last_local_ts_ns: match last_record {
                Some(h) => Some(h.local_ts_ns).filter(|&ts| ts != 0),
                None => carried_ts,
            },
        };

        // Segments summary.
        let oldest_created_unix_ns = if first_base == newest_base {
            Some(header.created_unix_ns)
        } else {
            File::open(first_path)
                .ok()
                .and_then(|f| journal::read_header(&f, first_path, *first_base).ok())
                .map(|h| h.created_unix_ns)
        };
        let segments = Segments {
            count: sizes.len() as u64,
            first_seq: *first_base,
            total_bytes: sizes
                .iter()
                .fold(0u64, |sum, (_, s)| sum.saturating_add(*s)),
            oldest_created_unix_ns,
            before_checkpoint: marker.map_or(0, |(c, _)| {
                sizes
                    .iter()
                    .filter(|(b, _)| *b < c.segment_base_seq)
                    .count() as u64
            }),
        };

        // Checkpoint and exposure.
        let checkpoint = marker.map(|(slot, written)| {
            let (age, clock_skew) = match written {
                Some(w) if w > now_ns => (Some(Duration::ZERO), true),
                Some(w) => (Some(Duration::from_nanos(now_ns - w)), false),
                None => (None, false),
            };
            CheckpointStatus {
                marker: slot,
                written_unix_ns: written,
                age,
                clock_skew,
            }
        });
        let covered_from = match &checkpoint {
            Some(c) => Position::new(
                c.marker.next_seq,
                c.marker.segment_base_seq,
                c.marker.offset,
            ),
            None => Position::new(*first_base, *first_base, format::SEGMENT_HEADER_LEN as u64),
        };
        let uncovered = head.position.next_seq.saturating_sub(covered_from.next_seq);
        let generation = checkpoint.map_or(0, |c| c.marker.generation);
        let cached = self
            .previous
            .as_ref()
            .and_then(|p| p.exposure_ts)
            .filter(|&(g, seq, _)| g == generation && seq == covered_from.next_seq)
            .map(|(_, _, ts)| ts);
        let oldest_local_ts_ns = if uncovered == 0 {
            None
        } else if start == covered_from && first_record.is_some() {
            first_record.map(|h| h.local_ts_ns).filter(|&ts| ts != 0)
        } else if let Some(ts) = cached {
            ts
        } else {
            record_at(&self.dir, covered_from)
                .map(|h| h.local_ts_ns)
                .filter(|&ts| ts != 0)
        };
        let exposure = Exposure {
            records: uncovered,
            bytes: distance(&sizes, covered_from, head.position),
            age: if uncovered == 0 {
                Some(Duration::ZERO)
            } else {
                checkpoint.and_then(|c| c.age)
            },
            oldest_local_ts_ns,
        };

        // Rates and storage.
        let rates = self.previous.as_ref().and_then(|p| {
            let interval = at.checked_duration_since(p.at)?;
            let secs = interval.as_secs_f64();
            (secs > 0.0).then(|| Rates {
                interval,
                records_per_sec: head
                    .position
                    .next_seq
                    .saturating_sub(p.head.position.next_seq)
                    as f64
                    / secs,
                bytes_per_sec: distance(&sizes, p.head.position, head.position)
                    .map(|b| b as f64 / secs),
            })
        });
        let storage = storage(&self.dir, &head, rates)?;

        let writer = writer_process(&self.dir);
        let readers = read_beacons(&self.dir, &self.readers_dir, &head, &sizes, now_ns)?;

        self.previous = Some(Previous {
            at,
            head,
            identity,
            exposure_ts: (uncovered > 0).then_some((
                generation,
                covered_from.next_seq,
                oldest_local_ts_ns,
            )),
        });
        Ok(Status {
            dir: self.dir.clone(),
            sampled_unix_ns: now_ns,
            head,
            segments,
            checkpoint,
            exposure,
            storage,
            rates,
            writer,
            readers,
            sizes,
        })
    }
}

impl Reader {
    /// Where this reader's cursor is: the sequence number of the next record it will return
    /// ([`Reader::position`]), and the segment and offset it will read next. Pass it to [`Status::lag`], or publish
    /// it with a [`ReaderBeacon`].
    ///
    /// After a [`Reader::seek`] to a record that is still ahead, `next_seq` is the target while the segment and
    /// offset are where the reader is skipping from, so byte lag over-states until it gets there.
    ///
    /// Cost: no system calls.
    pub fn cursor(&self) -> Position {
        let (next_seq, segment, offset) = self.cursor_parts();
        let base = segment
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(format::parse_segment_file_name)
            .unwrap_or(next_seq);
        Position::new(next_seq, base, offset)
    }
}

/// Publishes a reader's position for observers in other processes ([`status`], `tickertape status`). Opt-in.
///
/// A beacon is a 64-byte file, by default `<journal>/readers/<name>`, outside the journal format (ADR 0032). The
/// beacon holds an exclusive `flock` on it while it exists, so an observer can tell a live reader from one that died
/// (the kernel releases the lock with the process): a dead reader's beacon shows as [`BeaconState::Stale`] and never
/// counts as lag. Dropping the beacon removes the file. The writer, other readers and retention never read beacons:
/// a slow or dead reader can't slow the writer down or hold back retention.
///
/// [`ReaderBeacon::update`] is meant to be called often (for example after each record or batch, and when a wait
/// times out): it writes at most once per interval, and only when the position moved. Call
/// [`ReaderBeacon::flush`] before a reader stops for a long time, so the last position is published.
///
/// ```no_run
/// # use std::time::Duration;
/// # use tickertape::{Reader, ReaderConfig, status::ReaderBeacon};
/// # fn main() -> tickertape::Result<()> {
/// let mut reader = Reader::open("/tmp/journal", ReaderConfig::default())?;
/// let mut beacon = ReaderBeacon::create("/tmp/journal", "risk-engine", &reader)?;
/// loop {
///     if let Some(record) = reader.next_timeout(Some(Duration::from_secs(1)))? {
///         // ... process the record ...
///         let _ = record;
///     }
///     beacon.update(&reader)?;
/// }
/// # }
/// ```
#[derive(Debug)]
pub struct ReaderBeacon {
    path: PathBuf,
    file: File,
    identity: Identity,
    interval: Duration,
    last_write: Option<Instant>,
    last_position: Option<Position>,
}

impl ReaderBeacon {
    /// Creates the beacon `name` in `<journal_dir>/readers` (creating that directory if needed), and publishes
    /// `reader`'s current position.
    ///
    /// `name` is 1 to 64 ASCII letters, digits, `.`, `_` or `-`, and doesn't start with `.`. A stale beacon with the
    /// same name (its owner died) is taken over.
    ///
    /// Cost: `mkdir`, `open`, `flock`, `ftruncate` and one 64-byte `pwrite`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidConfig`] for an invalid name or if the path isn't a regular file; [`Error::Io`], with kind
    /// [`std::io::ErrorKind::WouldBlock`] if a live beacon already has this name.
    pub fn create(
        journal_dir: impl AsRef<Path>,
        name: &str,
        reader: &Reader,
    ) -> Result<ReaderBeacon> {
        let dir = journal_dir.as_ref().join(READERS_DIR);
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(Error::io("create reader beacon directory", &dir, e)),
        }
        ReaderBeacon::create_in(dir, name, reader)
    }

    /// Like [`ReaderBeacon::create`], but in the existing directory `beacons_dir`, for readers that can't write to
    /// the journal directory. Point observers at it with [`Sampler::readers_dir`] or `tickertape status
    /// --readers-dir`.
    ///
    /// # Errors
    ///
    /// As for [`ReaderBeacon::create`].
    pub fn create_in(
        beacons_dir: impl AsRef<Path>,
        name: &str,
        reader: &Reader,
    ) -> Result<ReaderBeacon> {
        if !valid_beacon_name(name) {
            return Err(Error::InvalidConfig(format!(
                "reader beacon name {name:?}: use 1 to {BEACON_NAME_MAX} ASCII letters, digits, '.', '_' or '-', not starting with '.'"
            )));
        }
        let path = beacons_dir.as_ref().join(name);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o644)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| Error::io("open reader beacon", &path, e))?;
        let meta = file
            .metadata()
            .map_err(|e| Error::io("stat reader beacon", &path, e))?;
        if !meta.is_file() {
            return Err(Error::InvalidConfig(format!(
                "reader beacon {} is not a regular file",
                path.display()
            )));
        }
        // An observer's probe holds a shared lock for a few microseconds; retry briefly before giving up.
        let mut attempt = 0;
        loop {
            match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => break,
                Err(e) if e == rustix::io::Errno::WOULDBLOCK && attempt < 10 => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => return Err(Error::io("lock reader beacon", &path, e.into())),
            }
        }
        file.set_len(BEACON_LEN as u64)
            .map_err(|e| Error::io("size reader beacon", &path, e))?;
        let identity = journal::file_identity(&file, &path)?;
        let mut beacon = ReaderBeacon {
            path,
            file,
            identity,
            interval: DEFAULT_BEACON_INTERVAL,
            last_write: None,
            last_position: None,
        };
        beacon.write(reader.cursor())?;
        Ok(beacon)
    }

    /// Sets the minimum time between two writes made by [`ReaderBeacon::update`] (default
    /// [`DEFAULT_BEACON_INTERVAL`]). Zero writes on every position change.
    pub fn set_interval(&mut self, interval: Duration) {
        self.interval = interval;
    }

    /// Publishes `reader`'s position if its sequence number ([`Reader::position`]) moved since the last write and the
    /// interval has passed. Returns whether it wrote.
    ///
    /// Cost: a comparison if the position hasn't moved; otherwise one monotonic clock read, and one 64-byte `pwrite`
    /// (no sync) at most once per interval. Calling it after every record adds about one clock read per record;
    /// call it once per batch where that matters.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the write fails.
    pub fn update(&mut self, reader: &Reader) -> Result<bool> {
        if !self.due(reader.position()) {
            return Ok(false);
        }
        self.write(reader.cursor())?;
        Ok(true)
    }

    /// Like [`ReaderBeacon::update`], with a position the caller tracks itself.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the write fails.
    pub fn update_position(&mut self, position: Position) -> Result<bool> {
        if !self.due(position.next_seq) {
            return Ok(false);
        }
        self.write(position)?;
        Ok(true)
    }

    /// Whether a write is due for a reader now at `next_seq`.
    fn due(&self, next_seq: u64) -> bool {
        self.last_position.is_none_or(|p| p.next_seq != next_seq)
            && self
                .last_write
                .is_none_or(|at| at.elapsed() >= self.interval)
    }

    /// Publishes `reader`'s position now, whatever the interval.
    ///
    /// Cost: one 64-byte `pwrite`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the write fails.
    pub fn flush(&mut self, reader: &Reader) -> Result<()> {
        self.write(reader.cursor())
    }

    /// The beacon file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write(&mut self, position: Position) -> Result<()> {
        let bytes = encode_beacon(std::process::id(), position, crate::clock::realtime_ns());
        self.file
            .write_all_at(&bytes, 0)
            .map_err(|e| Error::io("write reader beacon", &self.path, e))?;
        self.last_write = Some(Instant::now());
        self.last_position = Some(position);
        Ok(())
    }
}

impl Drop for ReaderBeacon {
    fn drop(&mut self) {
        // Only remove the file if the name still refers to ours (someone may have replaced it).
        if journal::path_identity(&self.path) == Some(self.identity) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn valid_beacon_name(name: &str) -> bool {
    (1..=BEACON_NAME_MAX).contains(&name.len())
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Encodes a beacon (ADR 0032): magic, version, pid, position, update time, reserved zeros, CRC32C of bytes 0–59.
fn encode_beacon(pid: u32, position: Position, updated_unix_ns: u64) -> [u8; BEACON_LEN] {
    let mut b = [0u8; BEACON_LEN];
    b[0..8].copy_from_slice(&BEACON_MAGIC);
    b[8..12].copy_from_slice(&BEACON_VERSION.to_le_bytes());
    b[12..16].copy_from_slice(&pid.to_le_bytes());
    b[16..24].copy_from_slice(&position.next_seq.to_le_bytes());
    b[24..32].copy_from_slice(&position.segment_base_seq.to_le_bytes());
    b[32..40].copy_from_slice(&position.offset.to_le_bytes());
    b[40..48].copy_from_slice(&updated_unix_ns.to_le_bytes());
    let crc = crate::crc::crc32c(&b[..60]);
    b[60..64].copy_from_slice(&crc.to_le_bytes());
    b
}

fn decode_beacon(b: &[u8; BEACON_LEN]) -> Option<(u32, Position, u64)> {
    let u32_at = |at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    let u64_at = |at: usize| {
        let mut x = [0u8; 8];
        x.copy_from_slice(&b[at..at + 8]);
        u64::from_le_bytes(x)
    };
    if b[0..8] != BEACON_MAGIC
        || u32_at(8) != BEACON_VERSION
        || b[48..60].iter().any(|&x| x != 0)
        || u32_at(60) != crate::crc::crc32c(&b[..60])
    {
        return None;
    }
    Some((
        u32_at(12),
        Position::new(u64_at(16), u64_at(24), u64_at(32)),
        u64_at(40),
    ))
}

/// Whether `offset` can hold a slot in a segment of `size` bytes.
fn valid_offset(offset: u64, size: u64) -> bool {
    offset % format::RECORD_ALIGN as u64 == 0
        && offset >= format::SEGMENT_HEADER_LEN as u64
        && offset <= size.saturating_sub(format::SEGMENT_TRAILER_LEN as u64)
}

/// Segment-file bytes from `from` to `to`: the rest of `from`'s segment, every segment in between, and `to`'s offset.
/// Zero if `to` is not after `from`; `None` if a segment involved isn't in `sizes`.
fn distance(sizes: &[(u64, u64)], from: Position, to: Position) -> Option<u64> {
    if from.segment_base_seq == to.segment_base_seq {
        return Some(to.offset.saturating_sub(from.offset));
    }
    if from.segment_base_seq > to.segment_base_seq {
        return Some(0);
    }
    let i = sizes
        .binary_search_by_key(&from.segment_base_seq, |(b, _)| *b)
        .ok()?;
    let j = sizes
        .binary_search_by_key(&to.segment_base_seq, |(b, _)| *b)
        .ok()?;
    let rest = sizes[i].1.saturating_sub(from.offset);
    let between = sizes[i + 1..j]
        .iter()
        .fold(0u64, |sum, (_, s)| sum.saturating_add(*s));
    Some(rest.saturating_add(between).saturating_add(to.offset))
}

struct ScanOut {
    end_position: Position,
    end: HeadEnd,
    first: Option<RecordHeader>,
    last: Option<RecordHeader>,
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

/// Scans the newest segment forward from `start` to the first slot that holds no valid record.
fn scan_from(file: &File, path: &Path, size: u64, start: Position) -> Result<ScanOut> {
    let view = PreadView::new(file, size, journal::SCAN_WINDOW);
    let mut position = start;
    let mut first = None;
    let mut last = None;
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
    })
}

/// The header of the record at `position`, following one seal; `None` if there is no valid record there (yet) or it
/// can't be read. Informational only, so errors are not reported.
fn record_at(dir: &Path, mut position: Position) -> Option<RecordHeader> {
    for _ in 0..2 {
        let path = journal::segment_path(dir, position.segment_base_seq);
        let file = File::open(&path).ok()?;
        let len = file.metadata().ok()?.len();
        let view = PreadView::new(&file, len, RECORD_WINDOW);
        match read_slot_retrying(&view, &path, position.offset, position.next_seq).ok()? {
            Ok(Slot::Record { header, .. }) => return Some(header),
            Ok(Slot::Seal) => {
                position = Position::new(
                    position.next_seq,
                    position.next_seq,
                    format::SEGMENT_HEADER_LEN as u64,
                );
            }
            _ => return None,
        }
    }
    None
}

/// Reads the checkpoint marker's two slots and its modification time.
fn read_marker(dir: &Path) -> Result<Option<(CheckpointSlot, Option<u64>)>> {
    let path = dir.join(journal::CHECKPOINT_FILE);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io("open checkpoint", &path, e)),
    };
    let meta = file
        .metadata()
        .map_err(|e| Error::io("stat checkpoint", &path, e))?;
    let len = meta.len().min(format::CHECKPOINT_FILE_LEN as u64) as usize;
    let mut bytes = vec![0u8; len];
    for at in format::CHECKPOINT_SLOT_OFFSETS {
        let end = at + format::CHECKPOINT_SLOT_LEN;
        if end <= len {
            file.read_exact_at(&mut bytes[at..end], at as u64)
                .map_err(|e| Error::io("read checkpoint", &path, e))?;
        }
    }
    let Some(slot) = format::decode_checkpoint(&bytes) else {
        return Ok(None);
    };
    let written = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    Ok(Some((slot, written)))
}

fn storage(dir: &Path, head: &Head, rates: Option<Rates>) -> Result<Storage> {
    let vfs = rustix::fs::statvfs(dir)
        .map_err(|e| Error::io("statvfs journal directory", dir, e.into()))?;
    let available_bytes = vfs.f_bavail.saturating_mul(vfs.f_frsize);
    let total_bytes = vfs.f_blocks.saturating_mul(vfs.f_frsize);
    let segment_size = head.segment_size.max(1);
    let spare_segment = fs::symlink_metadata(dir.join(journal::SPARE_FILE))
        .is_ok_and(|m| m.is_file() && m.len() == segment_size);
    let rolls_left = available_bytes / segment_size + u64::from(spare_segment);
    let time_to_full = rates
        .and_then(|r| r.bytes_per_sec)
        .filter(|&rate| rate > 0.0)
        .and_then(|rate| {
            let left_in_active = segment_size.saturating_sub(head.position.offset);
            let bytes = left_in_active.saturating_add(rolls_left.saturating_mul(segment_size));
            Duration::try_from_secs_f64(bytes as f64 / rate).ok()
        });
    Ok(Storage {
        available_bytes,
        total_bytes,
        segment_size,
        spare_segment,
        rolls_left,
        time_to_full,
    })
}

/// Opens a file for reading without blocking on a FIFO and without following a symlink.
fn open_plain(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(file)
}

fn writer_process(dir: &Path) -> WriterProcess {
    use std::io::Read;

    let pid = open_plain(&dir.join(journal::LOCK_FILE))
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

fn read_beacons(
    dir: &Path,
    readers_dir: &Path,
    head: &Head,
    sizes: &[(u64, u64)],
    now_ns: u64,
) -> Result<Vec<ReaderStatus>> {
    let entries = match fs::read_dir(readers_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io("list reader beacons", readers_dir, e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io("list reader beacons", readers_dir, e))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !valid_beacon_name(&name) {
            continue;
        }
        let path = entry.path();
        let file = match open_plain(&path) {
            Ok(file) => file,
            // Removed meanwhile, not a regular file, or a symlink (`O_NOFOLLOW`): not a beacon.
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    || e.kind() == io::ErrorKind::InvalidInput
                    || e.raw_os_error() == Some(libc::ELOOP) =>
            {
                continue;
            }
            Err(e) => return Err(Error::io("open reader beacon", &path, e)),
        };
        let mut decoded = None;
        for _ in 0..2 {
            let mut bytes = [0u8; BEACON_LEN];
            if file.read_exact_at(&mut bytes, 0).is_err() {
                break;
            }
            decoded = decode_beacon(&bytes);
            if decoded.is_some() {
                break;
            }
        }
        let live = match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockShared)
        {
            Ok(()) => {
                let _ = rustix::fs::flock(&file, rustix::fs::FlockOperation::Unlock);
                false
            }
            Err(e) if e == rustix::io::Errno::WOULDBLOCK => true,
            // Locks unsupported here: can't tell, so treat it as live (lag thresholds then apply).
            Err(_) => true,
        };
        drop(file);
        let mut status = ReaderStatus {
            name,
            path,
            state: BeaconState::Invalid,
            pid: None,
            position: None,
            updated_unix_ns: None,
            update_age: None,
            lag: None,
            oldest_unread_local_ts_ns: None,
            unread_age: None,
        };
        if let Some((pid, position, updated)) = decoded {
            let lag = Lag {
                records: head.position.next_seq.saturating_sub(position.next_seq),
                bytes: distance(sizes, position, head.position),
            };
            let oldest_unread = if lag.records == 0 {
                None
            } else {
                record_at(dir, position)
                    .map(|h| h.local_ts_ns)
                    .filter(|&ts| ts != 0)
            };
            status.state = if live {
                BeaconState::Live
            } else {
                BeaconState::Stale
            };
            status.pid = Some(pid);
            status.position = Some(position);
            status.updated_unix_ns = Some(updated);
            status.update_age = Some(Duration::from_nanos(now_ns.saturating_sub(updated)));
            status.lag = Some(lag);
            status.oldest_unread_local_ts_ns = oldest_unread;
            status.unread_age = if lag.records == 0 {
                Some(Duration::ZERO)
            } else {
                oldest_unread.map(|ts| Duration::from_nanos(now_ns.saturating_sub(ts)))
            };
        }
        out.push(status);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

#[cfg(test)]
mod tests;
