//! Operational metrics (ADR 0032): the journal status sample that `tickertape status` prints, the writer's metrics and
//! a reader's lag. Read-only: nothing here is on the append or per-record read paths.

use std::ffi::{CString, c_char};
use std::time::Duration;

use tickertape::status::{
    AgeUnknown, BeaconDirState, BeaconState, CheckpointProblem, HeadEnd, LagUnknown, ReaderStatus,
    Sampler, Status,
};

use crate::args;
use crate::reader::{self, tickertape_reader};
use crate::status::tickertape_status_code as S;
use crate::status::{
    FfiError, corrupt_reason_of, guard, tickertape_corrupt_reason, tickertape_status,
};
use crate::writer::{self, tickertape_writer};

/// The value of an optional `uint64_t` field that the library could not establish (ADR 0032: nothing reported is
/// guessed). Fields that can hold it say so. A caller-supplied timestamp of `UINT64_MAX` (like one of 0, which means
/// "not set") also reads as this value.
pub const TICKERTAPE_UNKNOWN: u64 = 0xFFFF_FFFF_FFFF_FFFF;

/// How the newest segment ends, as `tickertape_journal_status_sample` saw it (`head_end`).
#[allow(non_camel_case_types)]
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum tickertape_head_end {
    /// At a slot with no record yet: the normal state.
    TICKERTAPE_HEAD_OPEN = 0,
    /// At a seal whose successor did not exist when the sample checked: a writer died during a roll. The next
    /// writer creates the successor (`docs/format.md` → Recovery).
    TICKERTAPE_HEAD_SEALED = 1,
    /// At bytes that still failed validation after re-reading (`head_corrupt_reason` says why): a corruption
    /// breach. Not possible after a writer process crash; seen after a power loss or storage corruption, until the
    /// next writer repairs it.
    TICKERTAPE_HEAD_INVALID = 2,
    /// A state this version of the C ABI does not name (not returned by a library built from one source tree).
    /// Treat it as unknown, not as a healthy head.
    TICKERTAPE_HEAD_OTHER = 255,
}

/// Why the checkpoint marker cannot be used (`checkpoint_problem`). Any value but `TICKERTAPE_CHECKPOINT_USABLE`
/// means that a writer refuses to open the journal under every recovery policy: a corruption breach.
#[allow(non_camel_case_types)]
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum tickertape_checkpoint_problem {
    /// No problem was found, or there is no checkpoint marker (`has_checkpoint` is 0).
    TICKERTAPE_CHECKPOINT_USABLE = 0,
    /// The marker's segment does not exist.
    TICKERTAPE_CHECKPOINT_MISSING_SEGMENT = 1,
    /// The marker's segment exists but its header is invalid.
    TICKERTAPE_CHECKPOINT_INVALID_SEGMENT_HEADER = 2,
    /// The marker's offset is misaligned or outside its segment's record area.
    TICKERTAPE_CHECKPOINT_INVALID_OFFSET = 3,
    /// A problem this version of the C ABI does not name (not returned by a library built from one source tree).
    TICKERTAPE_CHECKPOINT_OTHER_PROBLEM = 255,
}

/// Why the checkpoint's age could not be estimated from the marker's modification time (`checkpoint_age_unknown`).
#[allow(non_camel_case_types)]
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum tickertape_age_unknown {
    /// The age is known, or there is no checkpoint marker (`has_checkpoint` is 0).
    TICKERTAPE_AGE_KNOWN = 0,
    /// The filesystem does not report a modification time.
    TICKERTAPE_AGE_NO_MODIFICATION_TIME = 1,
    /// The modification time is later than the observer's wall clock.
    TICKERTAPE_AGE_IN_FUTURE = 2,
    /// The modification time is earlier than the creation time of the marker's segment, which a checkpoint cannot
    /// predate (a copy that did not keep times, or a clock step).
    TICKERTAPE_AGE_BEFORE_SEGMENT = 3,
    /// The marker file does not have its full length, so its time is when it was cut, not a checkpoint's.
    TICKERTAPE_AGE_MARKER_RESIZED = 4,
    /// The marker's metadata kept changing while it was read: a writer checkpointing very often.
    TICKERTAPE_AGE_CHANGING = 5,
    /// The marker lost bytes (its older slot does not hold the previous generation), so its time is not known to
    /// be a checkpoint's.
    TICKERTAPE_AGE_OLDER_SLOT_LOST = 6,
    /// A reason this version of the C ABI does not name (not returned by a library built from one source tree).
    TICKERTAPE_AGE_OTHER_REASON = 255,
}

/// Why a reader's lag is unknown (`lag_unknown`).
#[allow(non_camel_case_types)]
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum tickertape_lag_unknown {
    /// The lag is known (or, for a beacon, its contents could not be read: see `has_report`).
    TICKERTAPE_LAG_KNOWN = 0,
    /// The reader's segment is no longer in the journal: retention removed it, or recovery moved it to
    /// `quarantine/`.
    TICKERTAPE_LAG_SEGMENT_GONE = 1,
    /// The segment with the reader's base sequence number is a different file: another journal sharing the beacon
    /// directory, a journal replaced at the same path, or a segment that recovery repaired after the reader read it.
    TICKERTAPE_LAG_OTHER_HISTORY = 2,
    /// The reader's segment exists but its header could not be read.
    TICKERTAPE_LAG_SEGMENT_UNREADABLE = 3,
    /// A durable-only reader, and the checkpoint marker cannot be used.
    TICKERTAPE_LAG_CHECKPOINT_UNUSABLE = 4,
    /// The reader's segment is newer than every segment the sample listed: take a new sample.
    TICKERTAPE_LAG_AHEAD_OF_SAMPLE = 5,
    /// A reason this version of the C ABI does not name (not returned by a library built from one source tree).
    TICKERTAPE_LAG_OTHER_REASON = 255,
}

/// Whether a reader beacon's owner is still running, and whether the beacon could be read (`state`).
#[allow(non_camel_case_types)]
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum tickertape_beacon_state {
    /// A process holds the beacon's lock: its owner (or a child that inherited its descriptor).
    TICKERTAPE_BEACON_LIVE = 0,
    /// Nobody holds the lock: the owner died without removing the file. Its position is history, not lag.
    TICKERTAPE_BEACON_STALE = 1,
    /// A valid beacon whose lock state could not be established (the filesystem refused the probe).
    TICKERTAPE_BEACON_UNCERTAIN = 2,
    /// The file is not a valid beacon (length, magic, version or checksum), even after re-reading.
    TICKERTAPE_BEACON_INVALID = 3,
    /// The file could not be opened or read (for example another user's beacon created under umask 077).
    TICKERTAPE_BEACON_UNREADABLE = 4,
    /// The name was replaced by another file during every attempt to observe it: nothing is reported.
    TICKERTAPE_BEACON_CHANGING = 5,
    /// A state this version of the C ABI does not name (not returned by a library built from one source tree).
    /// Nothing is reported, as for `TICKERTAPE_BEACON_CHANGING`.
    TICKERTAPE_BEACON_OTHER = 255,
}

/// Whether the beacon directory could be listed (`readers_dir_state`).
#[allow(non_camel_case_types)]
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum tickertape_readers_dir_state {
    /// Listed (possibly empty).
    TICKERTAPE_READERS_DIR_LISTED = 0,
    /// It does not exist: no reader has created a beacon there.
    TICKERTAPE_READERS_DIR_MISSING = 1,
    /// It exists but could not be listed (not a directory, no permission, an I/O error). No beacons are reported.
    TICKERTAPE_READERS_DIR_UNUSABLE = 2,
    /// A state this version of the C ABI does not name (not returned by a library built from one source tree).
    TICKERTAPE_READERS_DIR_OTHER = 255,
}

/// One sample of a journal's operational state (an opaque, immutable handle): what `tickertape status` prints.
/// Take one with `tickertape_journal_status_sample`, free it with `tickertape_journal_status_free`.
///
/// The functions that take a `const tickertape_journal_status *` only read it, so several threads may call them on
/// one sample at once; `tickertape_journal_status_free` must not run at the same time as any of them. The sample does
/// not keep any file open.
#[allow(non_camel_case_types)]
#[derive(Debug)]
pub struct tickertape_journal_status {
    status: Status,
    /// The beacons' names, NUL-terminated, in the order of `status.readers`.
    names: Vec<CString>,
}

/// The values of a journal status sample, an extensible struct (ADR 0028 → Versioning), filled by
/// `tickertape_journal_status_get_report`.
///
/// Set `struct_size` to `sizeof(tickertape_journal_status_report)` before the call. The library rejects a
/// `struct_size` below 272 with `TICKERTAPE_E_INVALID_ARGUMENT`, writes `min(struct_size, the library's own size)`
/// bytes, and stores the number of bytes it filled back into `struct_size`. Later versions only append fields.
///
/// Fields marked "or `TICKERTAPE_UNKNOWN`" hold that value when the sample could not establish them. Ages and
/// timestamps are nanoseconds; ages are estimates from file times and the observer's wall clock (ADR 0032 → What is
/// measured). Booleans are 0 or 1.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct tickertape_journal_status_report {
    /// In: the size of the caller's struct. Out: the number of bytes the library filled.
    pub struct_size: u64,
    /// The observer's wall clock after reading the checkpoint marker and the beacons, nanoseconds since the Unix
    /// epoch. Ages are measured from it.
    pub sampled_unix_ns: u64,
    /// The head: the sequence number the next record will get, as far as the newest segment shows.
    pub head_next_seq: u64,
    /// Base sequence number of the newest segment.
    pub head_segment_base_seq: u64,
    /// Offset of the head in that segment.
    pub head_offset: u64,
    /// Size of the newest segment, in bytes.
    pub head_segment_size: u64,
    /// `local_ts_ns` of the last record the scan passed in the newest segment (caller-supplied, in the producer's
    /// clock), or `TICKERTAPE_UNKNOWN` if it passed none or the timestamp is not set.
    pub head_last_local_ts_ns: u64,
    /// What the sample found at the head: a `tickertape_head_end`.
    pub head_end: i32,
    /// The `tickertape_corrupt_reason` if `head_end` is `TICKERTAPE_HEAD_INVALID`, else `TICKERTAPE_CORRUPT_NONE`.
    pub head_corrupt_reason: i32,
    /// 1 if the sample validated every record from the checkpoint marker's position (without a marker, from the
    /// start of the oldest segment) to the head. 0 means the segments in between were not read (or the head is at
    /// invalid bytes): a missing or damaged one would not have been noticed, and 0 with no breach does **not** mean
    /// that a writer would open the journal.
    pub head_verified: u32,
    /// 1 if the segment listing alone proves a segment missing between the checkpoint (or the oldest segment) and
    /// the head: a writer refuses the journal (a corruption breach).
    pub head_chain_broken: u32,
    /// Number of segment files.
    pub segment_count: u64,
    /// Base sequence number of the oldest segment.
    pub first_seq: u64,
    /// Sum of the segment files' (preallocated) sizes.
    pub segments_total_bytes: u64,
    /// Creation time from the oldest segment's header (the writer's wall clock), or `TICKERTAPE_UNKNOWN` if its
    /// header could not be read.
    pub oldest_segment_created_unix_ns: u64,
    /// Segments older than the checkpoint marker's segment (the ones retention could remove). 0 without a marker.
    pub segments_before_checkpoint: u64,
    /// 1 if there is a valid checkpoint marker. If 0, the `checkpoint_*` values are `TICKERTAPE_UNKNOWN` and the
    /// `checkpoint_*` enums 0.
    pub has_checkpoint: u32,
    /// A `tickertape_checkpoint_problem`: why the marker cannot be used, if it cannot (a corruption breach).
    pub checkpoint_problem: i32,
    /// The marker's generation.
    pub checkpoint_generation: u64,
    /// The marker's `next_seq`: every record below it was durable when the marker was written.
    pub checkpoint_next_seq: u64,
    /// The segment of the marker's position.
    pub checkpoint_segment_base_seq: u64,
    /// The offset of the marker's position in that segment.
    pub checkpoint_offset: u64,
    /// The marker file's modification time, nanoseconds since the Unix epoch, or `TICKERTAPE_UNKNOWN`.
    pub checkpoint_modified_unix_ns: u64,
    /// Estimated time since the last checkpoint (the observer's clock minus `checkpoint_modified_unix_ns`), or
    /// `TICKERTAPE_UNKNOWN` with the reason in `checkpoint_age_unknown`.
    pub checkpoint_estimated_age_ns: u64,
    /// A `tickertape_age_unknown`: why `checkpoint_estimated_age_ns` is unknown.
    pub checkpoint_age_unknown: i32,
    /// 1 if the exposure values are counted over records the sample validated (`head_verified`); 0 if they are
    /// what the newest segment implies.
    pub exposure_verified: u32,
    /// Committed records a power loss now could remove (not covered by the marker), or `TICKERTAPE_UNKNOWN` (the
    /// marker cannot be used, a segment in between is proven missing, or the head is at invalid bytes).
    pub exposure_records: u64,
    /// Segment-file footprint between the marker's position and the head, or `TICKERTAPE_UNKNOWN`.
    pub exposure_bytes: u64,
    /// Estimated upper bound on how long the uncovered records have been exposed (0 if there are none), or
    /// `TICKERTAPE_UNKNOWN`.
    pub exposure_estimated_age_ns: u64,
    /// `local_ts_ns` of the oldest uncovered record (producer's clock), or `TICKERTAPE_UNKNOWN`.
    pub exposure_oldest_local_ts_ns: u64,
    /// Bytes available to unprivileged users on the journal's filesystem (`statvfs`).
    pub storage_available_bytes: u64,
    /// Size of that filesystem.
    pub storage_total_bytes: u64,
    /// Size assumed for the segments still to come (the prepared spare's, else the newest segment's).
    pub storage_next_segment_size: u64,
    /// Estimated number of further segment rolls that fit: `storage_available_bytes / storage_next_segment_size`.
    /// Optimistic on btrfs (ADR 0032 → Headroom).
    pub storage_estimated_rolls_left: u64,
    /// 1 if a fully allocated prepared next segment exists (not counted in `storage_estimated_rolls_left`).
    pub storage_spare_segment: u32,
    /// A `tickertape_readers_dir_state`: whether the beacon directory could be listed.
    pub readers_dir_state: i32,
    /// The PID the most recent writer recorded in `writer.lock` (it stays after that writer exits), or
    /// `TICKERTAPE_UNKNOWN`. Informational, never used for health.
    pub writer_pid: u64,
    /// 1 if a process with `writer_pid` exists (a PID can be reused), 0 if not, or `TICKERTAPE_UNKNOWN`.
    pub writer_alive: u64,
    /// Number of reader beacons in the beacon directory: valid indexes for `tickertape_journal_status_get_beacon`.
    pub beacon_count: u64,
}

/// A reader in another process, from its beacon (ADR 0032 → Reader positions from other processes), an extensible
/// struct filled by `tickertape_journal_status_get_beacon`.
///
/// Set `struct_size` to `sizeof(tickertape_beacon_status)` before the call. The library rejects a `struct_size`
/// below 120 with `TICKERTAPE_E_INVALID_ARGUMENT`, writes `min(struct_size, the library's own size)` bytes, and
/// stores the number of bytes it filled back into `struct_size`. Later versions only append fields.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct tickertape_beacon_status {
    /// In: the size of the caller's struct. Out: the number of bytes the library filled.
    pub struct_size: u64,
    /// A `tickertape_beacon_state`.
    pub state: i32,
    /// 1 if the beacon's contents were read (not for `TICKERTAPE_BEACON_INVALID`, `_UNREADABLE` or `_CHANGING`).
    /// If 0, the `report_*` values are `TICKERTAPE_UNKNOWN` (the flags 0) and so is every lag value.
    pub has_report: u32,
    /// 1 if a seek was pending at the last write: the byte lag and unread age are then unknown.
    pub report_seek_pending: u32,
    /// 1 if the reader is durable-only: its lag is measured against the checkpoint marker.
    pub report_durable_only: u32,
    /// A `tickertape_lag_unknown`: why `lag_records` is unknown for a beacon that could be read.
    pub lag_unknown: i32,
    /// Always 0.
    pub reserved: u32,
    /// The owner's PID as it recorded it (informational), or `TICKERTAPE_UNKNOWN`.
    pub report_pid: u64,
    /// Sequence number of the next record the reader returns, or `TICKERTAPE_UNKNOWN`.
    pub report_next_seq: u64,
    /// Sequence number expected at the slot the reader reads next (differs from `report_next_seq` while a seek is
    /// pending), or `TICKERTAPE_UNKNOWN`.
    pub report_slot_seq: u64,
    /// That slot's segment, or `TICKERTAPE_UNKNOWN`.
    pub report_segment_base_seq: u64,
    /// That slot's offset, or `TICKERTAPE_UNKNOWN`.
    pub report_offset: u64,
    /// The owner's wall-clock time of the last write, nanoseconds since the Unix epoch, or `TICKERTAPE_UNKNOWN`.
    pub report_updated_unix_ns: u64,
    /// Time since the last write by the observer's clock, or `TICKERTAPE_UNKNOWN` (no report, or a write time in
    /// the future).
    pub update_age_ns: u64,
    /// Records the reader could return now but had not at its last write, or `TICKERTAPE_UNKNOWN` (see
    /// `lag_unknown`).
    pub lag_records: u64,
    /// Segment-file footprint between the reader's cursor and its target, or `TICKERTAPE_UNKNOWN` (a pending seek,
    /// or a segment of either end not listed).
    pub lag_bytes: u64,
    /// `local_ts_ns` of the first record the reader has not returned, or `TICKERTAPE_UNKNOWN`.
    pub oldest_unread_local_ts_ns: u64,
    /// The observer's clock minus `oldest_unread_local_ts_ns` (0 if the reader has caught up), or
    /// `TICKERTAPE_UNKNOWN`. Meaningful when the producer stamps `local_ts_ns` from this host's wall clock.
    pub unread_age_ns: u64,
}

/// What only the writer knows, an extensible struct filled by `tickertape_writer_get_metrics`.
///
/// Set `struct_size` to `sizeof(tickertape_writer_metrics)` before the call. The library rejects a `struct_size`
/// below 112 with `TICKERTAPE_E_INVALID_ARGUMENT`, writes `min(struct_size, the library's own size)` bytes, and stores
/// the number of bytes it filled back into `struct_size`. Later versions only append fields.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct tickertape_writer_metrics {
    /// In: the size of the caller's struct. Out: the number of bytes the library filled.
    pub struct_size: u64,
    /// The sequence number the next append gets.
    pub head_next_seq: u64,
    /// Base sequence number of the active segment.
    pub head_segment_base_seq: u64,
    /// Where the next record will be written in that segment.
    pub head_offset: u64,
    /// Size of the active segment.
    pub segment_size: u64,
    /// 1 if there is a checkpoint marker (as this writer last wrote it or read it at open). If 0, the
    /// `checkpoint_*` values are `TICKERTAPE_UNKNOWN`.
    pub has_checkpoint: u32,
    /// 1 after a failed checkpoint: later checkpoints fail with `TICKERTAPE_E_DURABILITY_LOST`.
    pub poisoned: u32,
    /// The marker's generation.
    pub checkpoint_generation: u64,
    /// The marker's `next_seq`.
    pub checkpoint_next_seq: u64,
    /// The segment of the marker's position.
    pub checkpoint_segment_base_seq: u64,
    /// The offset of the marker's position.
    pub checkpoint_offset: u64,
    /// Nanoseconds since this writer's last successful checkpoint by the monotonic clock (immune to wall-clock
    /// steps), or `TICKERTAPE_UNKNOWN` if this writer has not checkpointed yet.
    pub since_checkpoint_ns: u64,
    /// Records appended but not covered by the marker (with no marker, every record from the start of the oldest
    /// segment when the writer opened).
    pub uncheckpointed_records: u64,
    /// Bytes of records and seals appended after the marker, in every segment: what a power loss could remove.
    pub uncheckpointed_bytes: u64,
    /// Retention runs that failed since open; the last error is `tickertape_writer_last_retention_error`.
    pub retention_failures: u64,
}

/// A reader's cursor and its lag behind a journal status sample, an extensible struct filled by
/// `tickertape_reader_get_metrics`.
///
/// Set `struct_size` to `sizeof(tickertape_reader_metrics)` before the call. The library rejects a `struct_size`
/// below 72 with `TICKERTAPE_E_INVALID_ARGUMENT`, writes `min(struct_size, the library's own size)` bytes, and stores
/// the number of bytes it filled back into `struct_size`. Later versions only append fields.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct tickertape_reader_metrics {
    /// In: the size of the caller's struct. Out: the number of bytes the library filled.
    pub struct_size: u64,
    /// Sequence number of the next record the reader returns (as `tickertape_reader_position`).
    pub next_seq: u64,
    /// Sequence number expected at the slot the reader reads next. Equal to `next_seq` unless a seek is pending,
    /// or the handle holds a record back (`held_record`), when it is `next_seq + 1`.
    pub slot_seq: u64,
    /// That slot's segment.
    pub segment_base_seq: u64,
    /// That slot's offset.
    pub offset: u64,
    /// 1 while the target of a seek is still ahead of the slot (the byte lag is then unknown).
    pub seek_pending: u32,
    /// 1 if the reader is durable-only: its lag is measured against the checkpoint marker, not the head.
    pub durable_only: u32,
    /// 1 if the handle holds a record that did not fit the caller's buffer (`tickertape_reader_next_copy`,
    /// `tickertape_reader_read_batch`); the next read returns it. It counts in `lag_records`.
    pub held_record: u32,
    /// A `tickertape_lag_unknown`: why `lag_records` is unknown.
    pub lag_unknown: i32,
    /// Records the reader could return now but has not (committed records, or for a durable-only reader, records
    /// covered by the marker), as of the sample, or `TICKERTAPE_UNKNOWN` (see `lag_unknown`).
    pub lag_records: u64,
    /// Segment-file footprint between the cursor and its target (0 when `lag_records` is), or `TICKERTAPE_UNKNOWN`
    /// (a pending seek, a held record, or a segment of either end not listed).
    pub lag_bytes: u64,
}

/// The smallest accepted `struct_size` of each extensible struct: its size when it was added. (Functions, not
/// constants, so that cbindgen skips them.)
const fn journal_status_report_min_size() -> usize {
    272
}
const fn beacon_status_min_size() -> usize {
    120
}
const fn writer_metrics_min_size() -> usize {
    112
}
const fn reader_metrics_min_size() -> usize {
    72
}

// The layouts (ADR 0028 → Versioning); tests/c/test_api.c asserts the same values with offsetof and sizeof. The
// extensible structs have no padding, which `args::write_struct` relies on.
#[allow(dead_code)]
fn layouts_are_fixed() {
    // Inside a function so that cbindgen does not parse it; the constant is evaluated all the same.
    const _: () = {
        use std::mem::{align_of, offset_of, size_of};
        type J = tickertape_journal_status_report;
        assert!(size_of::<J>() == journal_status_report_min_size() && align_of::<J>() == 8);
        assert!(offset_of!(J, head_last_local_ts_ns) == 48);
        assert!(offset_of!(J, head_end) == 56 && offset_of!(J, head_corrupt_reason) == 60);
        assert!(offset_of!(J, head_verified) == 64 && offset_of!(J, head_chain_broken) == 68);
        assert!(offset_of!(J, segment_count) == 72);
        assert!(offset_of!(J, has_checkpoint) == 112 && offset_of!(J, checkpoint_problem) == 116);
        assert!(
            offset_of!(J, checkpoint_age_unknown) == 168 && offset_of!(J, exposure_verified) == 172
        );
        assert!(
            offset_of!(J, storage_spare_segment) == 240 && offset_of!(J, readers_dir_state) == 244
        );
        assert!(offset_of!(J, beacon_count) == 264);

        type B = tickertape_beacon_status;
        assert!(size_of::<B>() == beacon_status_min_size() && align_of::<B>() == 8);
        assert!(offset_of!(B, state) == 8 && offset_of!(B, has_report) == 12);
        assert!(offset_of!(B, lag_unknown) == 24 && offset_of!(B, reserved) == 28);
        assert!(offset_of!(B, report_pid) == 32 && offset_of!(B, unread_age_ns) == 112);

        type W = tickertape_writer_metrics;
        assert!(size_of::<W>() == writer_metrics_min_size() && align_of::<W>() == 8);
        assert!(offset_of!(W, has_checkpoint) == 40 && offset_of!(W, poisoned) == 44);
        assert!(offset_of!(W, retention_failures) == 104);

        type R = tickertape_reader_metrics;
        assert!(size_of::<R>() == reader_metrics_min_size() && align_of::<R>() == 8);
        assert!(offset_of!(R, seek_pending) == 40 && offset_of!(R, lag_unknown) == 52);
        assert!(offset_of!(R, lag_bytes) == 64);
    };
}

/// A sample may be read by several threads at once (see `tickertape_journal_status`): this fails to compile if the
/// core's `Status` stops being `Sync`.
#[allow(dead_code)]
fn samples_can_be_shared_between_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<tickertape_journal_status>();
}

/// Nanoseconds, saturating below `TICKERTAPE_UNKNOWN`.
fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).map_or(TICKERTAPE_UNKNOWN - 1, |n| n.min(TICKERTAPE_UNKNOWN - 1))
}

/// An optional value, `TICKERTAPE_UNKNOWN` when absent.
fn known(value: Option<u64>) -> u64 {
    value.unwrap_or(TICKERTAPE_UNKNOWN)
}

fn known_ns(value: Option<Duration>) -> u64 {
    value.map_or(TICKERTAPE_UNKNOWN, nanos)
}

fn flag(value: bool) -> u32 {
    u32::from(value)
}

pub(crate) fn lag_unknown_of(reason: LagUnknown) -> tickertape_lag_unknown {
    use tickertape_lag_unknown as L;
    match reason {
        LagUnknown::SegmentGone => L::TICKERTAPE_LAG_SEGMENT_GONE,
        LagUnknown::OtherHistory => L::TICKERTAPE_LAG_OTHER_HISTORY,
        LagUnknown::SegmentUnreadable => L::TICKERTAPE_LAG_SEGMENT_UNREADABLE,
        LagUnknown::CheckpointUnusable => L::TICKERTAPE_LAG_CHECKPOINT_UNUSABLE,
        LagUnknown::AheadOfSample => L::TICKERTAPE_LAG_AHEAD_OF_SAMPLE,
        // A reason added to the core after this mapping (the lag itself stays TICKERTAPE_UNKNOWN).
        _ => L::TICKERTAPE_LAG_OTHER_REASON,
    }
}

/// The C values of how the head ends, and the corruption reason for invalid bytes.
pub(crate) fn head_end_of(end: HeadEnd) -> (tickertape_head_end, tickertape_corrupt_reason) {
    use tickertape_corrupt_reason as R;
    use tickertape_head_end as H;
    match end {
        HeadEnd::Open => (H::TICKERTAPE_HEAD_OPEN, R::TICKERTAPE_CORRUPT_NONE),
        HeadEnd::Sealed => (H::TICKERTAPE_HEAD_SEALED, R::TICKERTAPE_CORRUPT_NONE),
        HeadEnd::Invalid(reason) => (H::TICKERTAPE_HEAD_INVALID, corrupt_reason_of(&reason)),
        // Added to the core after this mapping (the tests name every variant).
        _ => (H::TICKERTAPE_HEAD_OTHER, R::TICKERTAPE_CORRUPT_NONE),
    }
}

pub(crate) fn checkpoint_problem_of(
    problem: Option<CheckpointProblem>,
) -> tickertape_checkpoint_problem {
    use tickertape_checkpoint_problem as P;
    match problem {
        None => P::TICKERTAPE_CHECKPOINT_USABLE,
        Some(CheckpointProblem::MissingSegment) => P::TICKERTAPE_CHECKPOINT_MISSING_SEGMENT,
        Some(CheckpointProblem::InvalidSegmentHeader) => {
            P::TICKERTAPE_CHECKPOINT_INVALID_SEGMENT_HEADER
        }
        Some(CheckpointProblem::InvalidOffset) => P::TICKERTAPE_CHECKPOINT_INVALID_OFFSET,
        // Added to the core after this mapping: never reported as usable.
        Some(_) => P::TICKERTAPE_CHECKPOINT_OTHER_PROBLEM,
    }
}

pub(crate) fn age_unknown_of(reason: Option<AgeUnknown>) -> tickertape_age_unknown {
    use tickertape_age_unknown as A;
    match reason {
        None => A::TICKERTAPE_AGE_KNOWN,
        Some(AgeUnknown::NoModificationTime) => A::TICKERTAPE_AGE_NO_MODIFICATION_TIME,
        Some(AgeUnknown::InFuture) => A::TICKERTAPE_AGE_IN_FUTURE,
        Some(AgeUnknown::BeforeSegment) => A::TICKERTAPE_AGE_BEFORE_SEGMENT,
        Some(AgeUnknown::MarkerResized) => A::TICKERTAPE_AGE_MARKER_RESIZED,
        Some(AgeUnknown::Changing) => A::TICKERTAPE_AGE_CHANGING,
        Some(AgeUnknown::OlderSlotLost) => A::TICKERTAPE_AGE_OLDER_SLOT_LOST,
        Some(_) => A::TICKERTAPE_AGE_OTHER_REASON,
    }
}

pub(crate) fn beacon_state_of(state: BeaconState) -> tickertape_beacon_state {
    use tickertape_beacon_state as B;
    match state {
        BeaconState::Live => B::TICKERTAPE_BEACON_LIVE,
        BeaconState::Stale => B::TICKERTAPE_BEACON_STALE,
        BeaconState::Uncertain => B::TICKERTAPE_BEACON_UNCERTAIN,
        BeaconState::Invalid => B::TICKERTAPE_BEACON_INVALID,
        BeaconState::Unreadable => B::TICKERTAPE_BEACON_UNREADABLE,
        BeaconState::Changing => B::TICKERTAPE_BEACON_CHANGING,
        // Added to the core after this mapping: reported with nothing that was seen, like `Changing`.
        _ => B::TICKERTAPE_BEACON_OTHER,
    }
}

pub(crate) fn readers_dir_state_of(state: BeaconDirState) -> tickertape_readers_dir_state {
    use tickertape_readers_dir_state as D;
    match state {
        BeaconDirState::Listed => D::TICKERTAPE_READERS_DIR_LISTED,
        BeaconDirState::Missing => D::TICKERTAPE_READERS_DIR_MISSING,
        BeaconDirState::Unusable(_) => D::TICKERTAPE_READERS_DIR_UNUSABLE,
        // Added to the core after this mapping.
        _ => D::TICKERTAPE_READERS_DIR_OTHER,
    }
}

fn report_of(status: &Status, beacon_count: usize) -> tickertape_journal_status_report {
    let head = &status.head;
    let (head_end, head_reason) = head_end_of(head.end);
    let mut out = tickertape_journal_status_report {
        struct_size: 0,
        sampled_unix_ns: status.sampled_unix_ns,
        head_next_seq: head.position.next_seq,
        head_segment_base_seq: head.position.segment_base_seq,
        head_offset: head.position.offset,
        head_segment_size: head.segment_size,
        head_last_local_ts_ns: known(head.last_local_ts_ns),
        head_end: head_end as i32,
        head_corrupt_reason: head_reason as i32,
        head_verified: flag(head.verified),
        head_chain_broken: flag(head.chain_broken),
        segment_count: status.segments.count,
        first_seq: status.segments.first_seq,
        segments_total_bytes: status.segments.total_bytes,
        oldest_segment_created_unix_ns: known(status.segments.oldest_created_unix_ns),
        segments_before_checkpoint: status.segments.before_checkpoint,
        has_checkpoint: 0,
        checkpoint_problem: checkpoint_problem_of(None) as i32,
        checkpoint_generation: TICKERTAPE_UNKNOWN,
        checkpoint_next_seq: TICKERTAPE_UNKNOWN,
        checkpoint_segment_base_seq: TICKERTAPE_UNKNOWN,
        checkpoint_offset: TICKERTAPE_UNKNOWN,
        checkpoint_modified_unix_ns: TICKERTAPE_UNKNOWN,
        checkpoint_estimated_age_ns: TICKERTAPE_UNKNOWN,
        checkpoint_age_unknown: age_unknown_of(None) as i32,
        exposure_verified: flag(status.exposure.verified),
        exposure_records: known(status.exposure.records),
        exposure_bytes: known(status.exposure.bytes),
        exposure_estimated_age_ns: known_ns(status.exposure.estimated_age),
        exposure_oldest_local_ts_ns: known(status.exposure.oldest_local_ts_ns),
        storage_available_bytes: status.storage.available_bytes,
        storage_total_bytes: status.storage.total_bytes,
        storage_next_segment_size: status.storage.next_segment_size,
        storage_estimated_rolls_left: status.storage.estimated_rolls_left,
        storage_spare_segment: flag(status.storage.spare_segment),
        readers_dir_state: readers_dir_state_of(status.readers_dir_state) as i32,
        writer_pid: known(status.writer.pid.map(u64::from)),
        writer_alive: known(status.writer.alive.map(u64::from)),
        beacon_count: beacon_count as u64,
    };
    if let Some(c) = &status.checkpoint {
        out.has_checkpoint = 1;
        out.checkpoint_problem = checkpoint_problem_of(c.problem) as i32;
        out.checkpoint_generation = c.marker.generation;
        out.checkpoint_next_seq = c.marker.next_seq;
        out.checkpoint_segment_base_seq = c.marker.segment_base_seq;
        out.checkpoint_offset = c.marker.offset;
        out.checkpoint_modified_unix_ns = known(c.modified_unix_ns);
        out.checkpoint_estimated_age_ns = known_ns(c.estimated_age);
        out.checkpoint_age_unknown = age_unknown_of(c.age_unknown) as i32;
    }
    out
}

fn beacon_of(r: &ReaderStatus) -> tickertape_beacon_status {
    let state = beacon_state_of(r.state);
    let mut out = tickertape_beacon_status {
        struct_size: 0,
        state: state as i32,
        has_report: 0,
        report_seek_pending: 0,
        report_durable_only: 0,
        lag_unknown: r
            .lag_unknown
            .map_or(tickertape_lag_unknown::TICKERTAPE_LAG_KNOWN, lag_unknown_of)
            as i32,
        reserved: 0,
        report_pid: TICKERTAPE_UNKNOWN,
        report_next_seq: TICKERTAPE_UNKNOWN,
        report_slot_seq: TICKERTAPE_UNKNOWN,
        report_segment_base_seq: TICKERTAPE_UNKNOWN,
        report_offset: TICKERTAPE_UNKNOWN,
        report_updated_unix_ns: TICKERTAPE_UNKNOWN,
        update_age_ns: known_ns(r.update_age),
        lag_records: known(r.lag.map(|l| l.records)),
        lag_bytes: known(r.lag.and_then(|l| l.bytes)),
        oldest_unread_local_ts_ns: known(r.oldest_unread_local_ts_ns),
        unread_age_ns: known_ns(r.unread_age),
    };
    if matches!(
        state,
        tickertape_beacon_state::TICKERTAPE_BEACON_CHANGING
            | tickertape_beacon_state::TICKERTAPE_BEACON_OTHER
    ) {
        // No contents and no lag, as the core reports `Changing` (and for a state added to the core later).
        out.update_age_ns = TICKERTAPE_UNKNOWN;
        out.lag_records = TICKERTAPE_UNKNOWN;
        out.lag_bytes = TICKERTAPE_UNKNOWN;
        out.oldest_unread_local_ts_ns = TICKERTAPE_UNKNOWN;
        out.unread_age_ns = TICKERTAPE_UNKNOWN;
        out.lag_unknown = tickertape_lag_unknown::TICKERTAPE_LAG_KNOWN as i32;
        return out;
    }
    if let Some(report) = &r.report {
        out.has_report = 1;
        out.report_seek_pending = flag(report.seek_pending);
        out.report_durable_only = flag(report.durable_only);
        out.report_pid = u64::from(report.pid);
        out.report_next_seq = report.next_seq;
        out.report_slot_seq = report.position.next_seq;
        out.report_segment_base_seq = report.position.segment_base_seq;
        out.report_offset = report.position.offset;
        out.report_updated_unix_ns = report.updated_unix_ns;
    }
    out
}

/// Borrows a journal status sample.
///
/// # Safety
///
/// `ptr` must be NULL or a live sample from `tickertape_journal_status_sample` that is not freed during the borrow.
unsafe fn sample<'a>(
    ptr: *const tickertape_journal_status,
) -> Result<&'a tickertape_journal_status, FfiError> {
    if ptr.is_null() {
        return Err(FfiError::null("status"));
    }
    // SAFETY: not NULL; the caller guarantees a live sample, which is never mutated after creation.
    Ok(unsafe { &*ptr })
}

/// Takes one read-only sample of the journal in directory `dir` (ADR 0032; what `tickertape status` prints) and
/// stores a new handle in `*status_out` (NULL on failure). No writer is needed and none is disturbed: the sample
/// takes no lock anybody waits for, never writes, and opens every file non-blocking.
///
/// Beacons are read from `readers_dir`, or from `<dir>/readers` if it is NULL. A beacon that cannot be read, or a
/// beacon directory that cannot be listed, is reported in the sample and never fails it.
///
/// Cost (the full list is in ADR 0032 → Sampling cost): a listing of `dir`; the checkpoint marker (up to two 64-byte
/// `pread`s); a `pread` scan of the newest segment from the marker's position if the marker is in it, otherwise from
/// its first record, so the cost grows with what was appended since the last checkpoint; the headers of the oldest
/// segment and the marker's segment; `statvfs`; `writer.lock` (the PID, read without locking) and `kill(pid, 0)`; a
/// listing of the beacon directory and each beacon (129 bytes and a lock probe), plus one record read per lagging
/// beacon. Segments between the marker's segment and the newest are listed, not read. No file stays open after the
/// call.
///
/// Errors: `TICKERTAPE_E_NOT_FOUND` if `dir` has no segments; `TICKERTAPE_E_CORRUPT` if it has none but the marker
/// names one, or if the newest segment's header is invalid; `TICKERTAPE_E_UNSUPPORTED_VERSION`; `TICKERTAPE_E_IO`
/// (including when the writer rolled 16 times during one sample).
///
/// # Safety
///
/// `dir` must be NULL or a NUL-terminated string, and `readers_dir` NULL or a NUL-terminated string; both stay
/// unmodified until the call returns. `status_out` must be NULL or valid for writing a pointer, and must not overlap
/// either string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_journal_status_sample(
    dir: *const c_char,
    readers_dir: *const c_char,
    status_out: *mut *mut tickertape_journal_status,
) -> tickertape_status {
    guard(|| {
        if status_out.is_null() {
            return Err(FfiError::null("status_out"));
        }
        // SAFETY: not NULL; the caller guarantees it is writable and overlaps no input.
        unsafe { status_out.write(std::ptr::null_mut()) };
        // SAFETY: forwarded caller contract.
        let dir = unsafe { args::path(dir, "dir") }?;
        let mut sampler = Sampler::new(dir);
        if !readers_dir.is_null() {
            // SAFETY: forwarded caller contract.
            sampler = sampler.readers_dir(unsafe { args::path(readers_dir, "readers_dir") }?);
        }
        let status = sampler.sample()?;
        let names = status
            .readers
            .iter()
            .map(|r| {
                // A beacon name is 1 to 64 ASCII letters, digits, '.', '_' and '-' (ADR 0032), so it has no NUL.
                CString::new(r.name.as_bytes())
                    .map_err(|_| FfiError::invalid("a beacon name contains a NUL byte"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let handle = Box::into_raw(Box::new(tickertape_journal_status { status, names }));
        // SAFETY: checked at the start that `status_out` is writable.
        unsafe { status_out.write(handle) };
        Ok(S::TICKERTAPE_OK)
    })
}

/// Frees a journal status sample. NULL is ignored.
///
/// # Safety
///
/// `status` must be NULL or a live sample from `tickertape_journal_status_sample`, not in use by another thread.
/// Using or freeing it again afterwards is undefined behaviour; so is using a beacon name it returned.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_journal_status_free(status: *mut tickertape_journal_status) {
    if status.is_null() {
        return;
    }
    // SAFETY: the caller guarantees a live sample from `Box::into_raw` that nobody else uses; it is dropped once.
    // Dropping it runs no user code and cannot panic.
    drop(unsafe { Box::from_raw(status) });
}

/// Writes the values of a journal status sample to `*report`. No system calls.
///
/// Errors: `TICKERTAPE_E_INVALID_ARGUMENT` if a pointer is NULL or `report->struct_size` is below the minimum (see
/// `tickertape_journal_status_report`).
///
/// # Safety
///
/// `status` must be NULL or a live sample. `report` must be NULL or point to a caller-owned, 8-byte aligned struct
/// of `report->struct_size` writable bytes whose `struct_size` is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_journal_status_get_report(
    status: *const tickertape_journal_status,
    report: *mut tickertape_journal_status_report,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let filled =
            unsafe { args::struct_size(report, journal_status_report_min_size(), "report") }?;
        // SAFETY: forwarded caller contract.
        let sample = unsafe { sample(status) }?;
        let out = report_of(&sample.status, sample.names.len());
        // SAFETY: the caller guarantees `struct_size` writable bytes at `report`, and `filled` is at most that.
        unsafe { args::write_struct(report, out, filled) };
        Ok(S::TICKERTAPE_OK)
    })
}

/// Writes what beacon `index` (0 to `beacon_count - 1`, sorted by name) of a journal status sample says to
/// `*beacon`. No system calls.
///
/// Errors: `TICKERTAPE_E_INVALID_ARGUMENT` if a pointer is NULL, `index` is out of range, or `beacon->struct_size`
/// is below the minimum (see `tickertape_beacon_status`).
///
/// # Safety
///
/// `status` must be NULL or a live sample. `beacon` must be NULL or point to a caller-owned, 8-byte aligned struct
/// of `beacon->struct_size` writable bytes whose `struct_size` is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_journal_status_get_beacon(
    status: *const tickertape_journal_status,
    index: usize,
    beacon: *mut tickertape_beacon_status,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let filled = unsafe { args::struct_size(beacon, beacon_status_min_size(), "beacon") }?;
        // SAFETY: forwarded caller contract.
        let sample = unsafe { sample(status) }?;
        let Some(r) = sample.status.readers.get(index) else {
            return Err(FfiError::invalid(format!(
                "index {index} is out of range: the sample has {} beacons",
                sample.status.readers.len()
            )));
        };
        // SAFETY: the caller guarantees `struct_size` writable bytes at `beacon`, and `filled` is at most that.
        unsafe { args::write_struct(beacon, beacon_of(r), filled) };
        Ok(S::TICKERTAPE_OK)
    })
}

/// Returns the name of beacon `index` of a journal status sample (its file name in the beacon directory: 1 to 64
/// ASCII letters, digits, `.`, `_` and `-`), or NULL if `status` is NULL or `index` is out of range. The string
/// belongs to the sample and stays valid until it is freed. Does not set the last error.
///
/// # Safety
///
/// `status` must be NULL or a live sample.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_journal_status_beacon_name(
    status: *const tickertape_journal_status,
    index: usize,
) -> *const c_char {
    if status.is_null() {
        return std::ptr::null();
    }
    // SAFETY: not NULL; the caller guarantees a live sample, never mutated after creation.
    let sample = unsafe { &*status };
    sample
        .names
        .get(index)
        .map_or(std::ptr::null(), |n| n.as_ptr())
}

/// Writes the writer's metrics to `*metrics`: what only the writer knows (its head, the last checkpoint by the
/// monotonic clock, uncheckpointed records and bytes, whether it is poisoned, retention failures). No system calls
/// (one monotonic clock read).
///
/// Errors: `TICKERTAPE_E_INVALID_ARGUMENT` if a pointer is NULL or `metrics->struct_size` is below the minimum (see
/// `tickertape_writer_metrics`); `TICKERTAPE_E_BUSY` from a fill callback of the same writer.
///
/// # Safety
///
/// `writer` must be NULL or a live writer handle. `metrics` must be NULL or point to a caller-owned, 8-byte aligned
/// struct of `metrics->struct_size` writable bytes whose `struct_size` is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_writer_get_metrics(
    writer: *mut tickertape_writer,
    metrics: *mut tickertape_writer_metrics,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let filled = unsafe { args::struct_size(metrics, writer_metrics_min_size(), "metrics") }?;
        // SAFETY: forwarded caller contract.
        let m = unsafe { writer::writer_handle(writer) }?.with_ref(|w| Ok(w.metrics()))?;
        let mut out = tickertape_writer_metrics {
            struct_size: 0,
            head_next_seq: m.head.next_seq,
            head_segment_base_seq: m.head.segment_base_seq,
            head_offset: m.head.offset,
            segment_size: m.segment_size,
            has_checkpoint: 0,
            poisoned: flag(m.poisoned),
            checkpoint_generation: TICKERTAPE_UNKNOWN,
            checkpoint_next_seq: TICKERTAPE_UNKNOWN,
            checkpoint_segment_base_seq: TICKERTAPE_UNKNOWN,
            checkpoint_offset: TICKERTAPE_UNKNOWN,
            since_checkpoint_ns: known_ns(m.since_checkpoint),
            uncheckpointed_records: m.uncheckpointed_records,
            uncheckpointed_bytes: m.uncheckpointed_bytes,
            retention_failures: m.retention_failures,
        };
        if let Some(c) = m.checkpoint {
            out.has_checkpoint = 1;
            out.checkpoint_generation = c.generation;
            out.checkpoint_next_seq = c.next_seq;
            out.checkpoint_segment_base_seq = c.segment_base_seq;
            out.checkpoint_offset = c.offset;
        }
        // SAFETY: the caller guarantees `struct_size` writable bytes at `metrics`, and `filled` is at most that.
        unsafe { args::write_struct(metrics, out, filled) };
        Ok(S::TICKERTAPE_OK)
    })
}

/// Writes the reader's cursor and its lag behind the journal status sample `status` to `*metrics`: how many records
/// it could return now but has not, as of the sample (behind the head, or for a durable-only reader, behind the
/// checkpoint marker). The reader's segment must be the file the sample listed under that name; otherwise the lag is
/// unknown, with the reason in `lag_unknown`. Take the sample after the reader's last read for an up-to-date lag. No
/// system calls (a search of the sample's segment list). Does not invalidate a record view returned by
/// `tickertape_reader_next`.
///
/// Errors: `TICKERTAPE_E_INVALID_ARGUMENT` if a pointer is NULL or `metrics->struct_size` is below the minimum (see
/// `tickertape_reader_metrics`).
///
/// # Safety
///
/// `reader` must be NULL or a live reader handle, and `status` NULL or a live sample. `metrics` must be NULL or point
/// to a caller-owned, 8-byte aligned struct of `metrics->struct_size` writable bytes whose `struct_size` is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tickertape_reader_get_metrics(
    reader: *mut tickertape_reader,
    status: *const tickertape_journal_status,
    metrics: *mut tickertape_reader_metrics,
) -> tickertape_status {
    guard(|| {
        // SAFETY: forwarded caller contract.
        let filled = unsafe { args::struct_size(metrics, reader_metrics_min_size(), "metrics") }?;
        // SAFETY: forwarded caller contract.
        let sample = unsafe { sample(status) }?;
        // SAFETY: forwarded caller contract.
        let (cursor, held) =
            unsafe { reader::reader_handle(reader) }?.with_ref(|state| Ok(state.cursor()))?;
        let lag = sample.status.lag(&cursor);
        let mut out = tickertape_reader_metrics {
            struct_size: 0,
            next_seq: held.unwrap_or(cursor.next_seq),
            slot_seq: cursor.position.next_seq,
            segment_base_seq: cursor.position.segment_base_seq,
            offset: cursor.position.offset,
            seek_pending: flag(cursor.seek_pending),
            durable_only: flag(cursor.durable_only),
            held_record: flag(held.is_some()),
            lag_unknown: tickertape_lag_unknown::TICKERTAPE_LAG_KNOWN as i32,
            lag_records: TICKERTAPE_UNKNOWN,
            lag_bytes: TICKERTAPE_UNKNOWN,
        };
        match lag {
            Ok(lag) if held.is_some() => {
                // The held record was taken from the core reader, which has moved past it; the caller has not seen
                // it. Its footprint is not counted by the core, so the byte lag is left unknown.
                out.lag_records = lag.records.saturating_add(1);
            }
            Ok(lag) => {
                out.lag_records = lag.records;
                out.lag_bytes = known(lag.bytes);
            }
            Err(reason) => out.lag_unknown = lag_unknown_of(reason) as i32,
        }
        // SAFETY: the caller guarantees `struct_size` writable bytes at `metrics`, and `filled` is at most that.
        unsafe { args::write_struct(metrics, out, filled) };
        Ok(S::TICKERTAPE_OK)
    })
}
