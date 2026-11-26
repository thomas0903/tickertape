//! Opt-in removal of a sealed, checkpoint-covered prefix of a journal (ADR 0016).
//!
//! [`plan`] is read-only. [`apply`] takes no writer lock, rechecks eligibility before each removal, and stops on
//! the first error. Limits are best effort: the checkpoint segment and everything after it are always retained.
//! A move never replaces a destination file, even an identical copy left by an interrupted move. Resolve that
//! conflict before retrying, or use a fresh destination. Destination directories must already exist.
//!
//! What planning reads of a journal depends on the policy (ADR 0051, [`plan`]): the names of the segments, and the
//! headers of those the policy has to consider, never every header. A run lists the directory once.
//!
//! What is read of a segment before it goes (ADR 0044, crash contract C14): a deletion, and [`plan`], check its header
//! and its successor's, and that its last non-zero bytes are a valid seal naming the successor, without reading its
//! records; a move reads the whole segment and validates every record and the seal first, as the copy it leaves must be
//! a valid segment.
//!
//! An archiver other than the writer ([`apply`]) does not hold the writer lock, so a recovery can run between its checks
//! of a segment and the removal (issue #83). It takes the segment out of the journal by a rename, into the destination
//! or to a temporary name in the journal directory (`{base:020}.tape.{pid}.{unix_ns}.removing`), and checks the file it
//! took before it lets it go; if the check fails it renames the file back ([`apply`]).

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::format::{self, CheckpointSlot, ScanEnd};
use crate::journal::Listed;
use crate::{CorruptReason, Error, Result, inspect, journal, mmap};

/// Any configured limit may request removal. No limits means no removal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Policy {
    /// Maximum number of segment files, including the active segment.
    pub max_segments: Option<usize>,
    /// Maximum sum of segment file sizes (not occupied record bytes).
    pub max_bytes: Option<u64>,
    /// Remove segments sealed longer ago than this duration. Seal time is the successor's creation time.
    pub max_age: Option<Duration>,
}

impl Policy {
    /// Sets the maximum total number of segments. Zero still preserves the checkpoint and newer segments.
    pub fn max_segments(mut self, count: usize) -> Self {
        self.max_segments = Some(count);
        self
    }

    /// Sets the maximum total segment bytes.
    pub fn max_bytes(mut self, bytes: u64) -> Self {
        self.max_bytes = Some(bytes);
        self
    }

    /// Sets the maximum time since sealing. Future seal times do not qualify.
    pub fn max_age(mut self, age: Duration) -> Self {
        self.max_age = Some(age);
        self
    }
}

/// What to do with each eligible segment.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Action {
    /// Permanently delete the segment. Its seal is checked first, its records are not read ([`apply`]).
    Delete,
    /// Move it to an existing directory, preserving its basename. Existing destination entries are errors. Every
    /// record and the seal are read and validated first ([`apply`]).
    MoveTo(PathBuf),
}

/// A limit that requests removing a segment, evaluated after the preceding planned removals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reason {
    /// Too many segment files remain.
    MaxSegments,
    /// Total segment bytes exceed the limit.
    MaxBytes,
    /// The successor's creation time is older than the age limit.
    MaxAge,
}

/// One planned removal. Plans cannot be constructed or reordered by callers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PlannedSegment {
    /// Original segment path.
    pub path: PathBuf,
    /// First sequence number in this segment.
    pub base_seq: u64,
    /// Full segment size in bytes.
    pub size_bytes: u64,
    /// Seal time: the successor header's creation time, in Unix nanoseconds.
    pub sealed_unix_ns: u64,
    /// All limits requesting this removal, in count, bytes, age order.
    pub reasons: Vec<Reason>,
    identity: journal::Identity,
    successor: u64,
}

/// Read-only plan bound to the journal directory's identity, ordered oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    segments: Vec<PlannedSegment>,
    directory: journal::Identity,
    /// Temporary names of segments that removals took out of the journal and left there ([`Leftover`]).
    leftovers: Vec<Leftover>,
    /// The bases of the segments the plan's listing showed, kept only when there are leftovers.
    listed: Vec<u64>,
}

impl Plan {
    /// Segments to remove, oldest first, with the reason for each.
    pub fn segments(&self) -> &[PlannedSegment] {
        &self.segments
    }
}

/// Successful removals and observations of a concurrent archiver.
///
/// [`apply`] returns this on success. On error earlier removals remain in effect; it does not roll them back.
/// [`crate::Writer::retention_report`] accumulates successes even from partially failed runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Report {
    /// Files successfully removed from the journal by this caller (saturates at u64::MAX).
    pub removed_segments: u64,
    /// Sum of the full sizes of those files (saturates at u64::MAX).
    pub removed_bytes: u64,
    /// Planned files already absent at the oldest end (a concurrent archiver).
    pub already_removed_segments: u64,
    /// Temporary names of segments that interrupted removals left in the journal directory
    /// (`{base:020}.tape.{pid}.{unix_ns}.removing`, ADR 0055) and that this run left there: their segments may hold
    /// records the journal no longer has (`docs/operations.md` says what to do). In
    /// [`crate::Writer::retention_report`], those of the writer's last run.
    pub leftovers: Vec<PathBuf>,
}

impl Report {
    /// Adds another report's counts to this one (saturating), and takes its leftovers.
    pub(crate) fn absorb(&mut self, other: &Report) {
        self.removed_segments = self.removed_segments.saturating_add(other.removed_segments);
        self.removed_bytes = self.removed_bytes.saturating_add(other.removed_bytes);
        self.already_removed_segments = self
            .already_removed_segments
            .saturating_add(other.already_removed_segments);
        self.leftovers.clone_from(&other.leftovers);
    }
}

/// Plans eligible removals without changing files or taking the writer lock.
///
/// Only a contiguous sealed prefix strictly before a valid checkpoint segment qualifies. Age uses wall clock
/// time at planning; a nonmonotonic creation clock never allows skipping an ineligible oldest segment.
///
/// What it reads, besides one listing of the directory, the checkpoint marker and the header of the segment the marker
/// names (nothing but the listing when the policy has no limit):
///
/// - [`Policy::max_segments`]: nothing more while the number of names is within the limit.
/// - [`Policy::max_bytes`]: the size of every segment file (its metadata), and the header of each file whose size is
///   not confirmed otherwise: the marker's segment and those after it, and a file whose size is not a segment size or
///   differs from the size of the one before it. In a journal of one segment size, that is only the newest segments.
/// - [`Policy::max_age`]: the headers of the oldest segments, up to the first one that is young enough and its
///   successor, whose creation time says so.
/// - A combination of limits: what each of them needs.
///
/// It considers the segments from the oldest on, stopping at the first that no limit asks to remove (or that is not
/// before the marker's segment, or is the newest). Of each candidate, a segment it will list, it reads the header, the
/// successor's header, and the end of the file back to its seal (the bytes after the seal are zeros): a candidate is
/// sealed when its last non-zero eight bytes are a valid seal naming the next segment. Its records are not read, so a
/// damaged record inside a sealed candidate does not stop the plan; a move ([`Action::MoveTo`]) reads and validates the
/// whole segment before moving it. When the end of a candidate is not such a seal, the whole segment is scanned as
/// before, and the error names what the scan found.
///
/// So an invalid header, or a file that cannot be opened, is an error when it belongs to a candidate, to a candidate's
/// successor, to the marker's segment, (with `max_age`) to the first segment that is young enough or its successor, or
/// (with `max_bytes`) to a file whose header it reads as above, or whose metadata cannot be read. Other segments are not
/// opened, and damage in them does not stop retention (ADR 0051; up to 1.0 this read every header and failed for any
/// invalid one): run [`inspect::verify`] to find it. Where there is more than one such error, it is the one of the
/// oldest segment among those read.
///
/// # Errors
///
/// [`Error::NotFound`] for an empty journal; [`Error::Corrupt`] or [`Error::UnsupportedVersion`] for an invalid
/// header of a segment it reads (above), an invalid checkpoint position, or a candidate's invalid seal or chain; [`Error::Io`] for filesystem errors, including concurrent
/// changes while observing the directory. An absent/invalid checkpoint slot yields an empty plan. With any limit set, a
/// marker the writer refuses is an error, whatever the policy asks to remove: from 1.1 that includes one whose position the records of
/// its segment do not reach ([`CorruptReason::DanglingCheckpoint`] at that segment and the marker's offset, as
/// [`Writer::open`](crate::Writer::open) reports it; ADR 0056), for which it reads the record right before the
/// marker's offset.
pub fn plan(dir: impl AsRef<Path>, policy: &Policy) -> Result<Plan> {
    plan_at(
        dir.as_ref(),
        policy,
        crate::clock::realtime_ns(),
        &|_| false,
        &mut Sizes::default(),
    )
}

/// The sizes of the segment files a writer's last retention run saw, kept for [`Policy::max_bytes`] so that each run asks
/// only for the files it has not seen (ADR 0051).
///
/// A size is used only while the run's own listing shows the same name with the same inode number, as a regular file.
/// A file the listing no longer shows is forgotten, whoever removed it, so another process's retention or archive cannot
/// make a size stale, and a recovery, the only thing that replaces a segment, needs the lock the writer holds. A segment
/// file is never resized (`docs/format.md` → Segment file). **The exception:** a file changed without a new inode number
/// (written over in place, or removed and created again where the file system gives the new file the old number, as ext4
/// and xfs do) keeps its old size here until the segment is removed or the writer opens again. That is outside what a
/// journal allows (`docs/operations.md`: segment files are not edited); the sum is then off by the difference in size,
/// which only shifts which segments the limit asks to remove (nothing that protects a segment from removal uses a size),
/// and the segment's header is checked when the plan reaches it.
#[derive(Debug, Default)]
pub(crate) struct Sizes {
    /// (base, inode, size, header checked against that size), by base.
    known: Vec<(u64, u64, u64, bool)>,
}

impl Sizes {
    /// The size of each listed segment file, its metadata asked for once.
    ///
    /// So that a file whose length is not its header's size cannot make the limit remove more than the headers allow,
    /// the header of a file is read and checked against its length, once, when the file is the marker's segment or after
    /// it (`marker`), when its length is not a valid segment size, or when its length differs from the one before it.
    /// Every length summed is then one a header confirms, or equals the length before it: in a journal of one segment
    /// size, only the newest segments are read.
    ///
    /// On an error, the place of the segment and the error: the error opening the segment gives, where its metadata
    /// cannot be read (the same as when every header was read).
    fn of(
        &mut self,
        dir: &Path,
        listed: &[Listed],
        marker: u64,
    ) -> std::result::Result<Vec<u64>, (usize, Error)> {
        let mut known = self.known.iter().peekable();
        let mut sizes: Vec<u64> = Vec::with_capacity(listed.len());
        let mut keep = Vec::with_capacity(listed.len());
        for (i, segment) in listed.iter().enumerate() {
            while known.next_if(|(base, ..)| *base < segment.base).is_some() {}
            let seen = known
                .next_if(|(base, ..)| *base == segment.base)
                .filter(|(_, inode, ..)| Some(*inode) == segment.inode);
            let (size, mut checked) = match seen {
                Some((_, _, size, checked)) => (*size, *checked),
                None => {
                    let path = journal::segment_path(dir, segment.base);
                    crate::fault::note_access(crate::fault::Access::StatSegment);
                    match fs::metadata(&path) {
                        Ok(meta) => (meta.len(), false),
                        Err(e) => {
                            let error = match open_header(dir, segment.base) {
                                Err(opening) => opening,
                                Ok(_) => Error::io("stat segment", &path, e),
                            };
                            return Err((i, error));
                        }
                    }
                }
            };
            let differs = i.checked_sub(1).is_some_and(|before| sizes[before] != size);
            if !checked
                && (segment.base >= marker
                    || format::validate_segment_size(size).is_err()
                    || differs)
            {
                open_header(dir, segment.base).map_err(|e| (i, e))?;
                checked = true;
            }
            sizes.push(size);
            if let Some(inode) = segment.inode {
                keep.push((segment.base, inode, size, checked));
            }
        }
        self.known = keep;
        Ok(sizes)
    }
}

/// Plans and applies one retention run of the writer itself. With `stop` (the checkpoint thread), once at least one
/// segment is planned, planning and removing stop as soon as `stop(segments)` returns true for the number planned or
/// handled so far (ADR 0044).
///
/// The writer runs retention only after its own marker is durable, so the marker is not synced again here. The plan's
/// own listing serves the removals ([`Known`]): a run lists the directory once.
pub(crate) fn run(
    dir: &Path,
    policy: &Policy,
    action: &Action,
    report: &mut Report,
    sizes: &mut Sizes,
    stop: Option<&dyn Fn(usize) -> bool>,
) -> Result<()> {
    let now = crate::clock::realtime_ns();
    let mut reached = None;
    let plan = plan_with(
        dir,
        policy,
        now,
        stop.unwrap_or(&|_| false),
        sizes,
        &mut reached,
    )?;
    let known = Known::from_plan(&plan);
    apply_into(dir, &plan, action, report, known, false, stop, reached)
}

/// The segment at `base`, opened and its header validated, with the identity of the file.
fn open_header(dir: &Path, base: u64) -> Result<(format::SegmentHeader, journal::Identity)> {
    let path = journal::segment_path(dir, base);
    let file = open_segment("open segment", &path)?;
    Ok((
        journal::read_header(&file, &path, base)?,
        journal::file_identity(&file, &path)?,
    ))
}

fn open_segment(op: &'static str, path: &Path) -> Result<File> {
    crate::fault::note_access(crate::fault::Access::OpenSegment);
    File::open(path).map_err(|e| Error::io(op, path, e))
}

/// [`plan`] at the time `now`. Each step reads only what the limits need to decide it.
fn plan_at(
    dir: &Path,
    policy: &Policy,
    now: u64,
    stop: &dyn Fn(usize) -> bool,
    sizes: &mut Sizes,
) -> Result<Plan> {
    plan_with(dir, policy, now, stop, sizes, &mut None)
}

/// [`plan_at`], setting `reached` to the marker slot whose records it found reaching its position ([`reached_marker`]),
/// so that a writer's run does not read that record again before its removals.
fn plan_with(
    dir: &Path,
    policy: &Policy,
    now: u64,
    stop: &dyn Fn(usize) -> bool,
    sizes: &mut Sizes,
    reached: &mut Option<CheckpointSlot>,
) -> Result<Plan> {
    let directory = directory_identity(dir)?;
    let mut leftovers = Vec::new();
    let listed = journal::list_bases_and(dir, |name| leftovers.extend(Leftover::parse(name)))?;
    if listed.is_empty() {
        return Err(Error::NotFound { dir: dir.into() });
    }
    let listed_bases = if leftovers.is_empty() {
        Vec::new()
    } else {
        listed.iter().map(|s| s.base).collect()
    };
    let mut plan = Plan {
        segments: Vec::new(),
        directory,
        leftovers,
        listed: listed_bases,
    };
    if policy == &Policy::default() {
        return Ok(plan);
    }
    // A marker the writer would refuse removes nothing, whatever the policy (ADR 0056 amendment).
    let Some(checkpoint) = reached_marker(dir, inspect::read_checkpoint(dir)?, reached)? else {
        return Ok(plan);
    };
    let mut count = listed.len();
    let sizes = match policy.max_bytes {
        Some(_) => match sizes.of(dir, &listed, checkpoint.segment_base_seq) {
            Ok(sizes) => sizes,
            // The error of the oldest invalid header before it, if there is one, as when every header was read.
            Err((place, error)) => {
                for segment in &listed[..place] {
                    open_header(dir, segment.base)?;
                }
                return Err(error);
            }
        },
        None => Vec::new(),
    };
    let mut bytes: u128 = sizes.iter().map(|s| u128::from(*s)).sum();
    // 1. The candidates and their headers. A header is read only if a limit asks to remove the segment, or may do so by
    // age. Headers come before seals, so that a damaged header is reported before a damaged seal.
    let mut header = None;
    for (i, pair) in listed.windows(2).enumerate() {
        let (base, successor) = (pair[0].base, pair[1].base);
        if base >= checkpoint.segment_base_seq
            || (policy.max_age.is_none() && reasons(policy, count, bytes, now, now).is_empty())
        {
            break;
        }
        let (head, identity) = match header.take() {
            Some(read) => read,
            None => open_header(dir, base)?,
        };
        let next = open_header(dir, successor)?;
        let sealed = next.0.created_unix_ns;
        let reasons = reasons(policy, count, bytes, now, sealed);
        if reasons.is_empty() || (!plan.segments.is_empty() && stop(plan.segments.len())) {
            break;
        }
        plan.segments.push(PlannedSegment {
            path: journal::segment_path(dir, base),
            base_seq: base,
            size_bytes: head.segment_size,
            sealed_unix_ns: sealed,
            reasons,
            identity,
            successor,
        });
        header = Some(next);
        count -= 1;
        bytes -= u128::from(sizes.get(i).copied().unwrap_or(0));
    }
    // 2. Their seals.
    for i in 0..plan.segments.len() {
        if i > 0 && stop(i) {
            plan.segments.truncate(i);
            break;
        }
        let segment = &plan.segments[i];
        let file = open_segment("open segment", &segment.path)?;
        check_seal(
            &file,
            &segment.path,
            segment.size_bytes,
            segment.base_seq,
            segment.successor,
        )?;
    }
    Ok(plan)
}

fn reasons(policy: &Policy, count: usize, bytes: u128, now: u64, sealed: u64) -> Vec<Reason> {
    let mut out = Vec::new();
    if policy.max_segments.is_some_and(|max| count > max) {
        out.push(Reason::MaxSegments);
    }
    if policy.max_bytes.is_some_and(|max| bytes > u128::from(max)) {
        out.push(Reason::MaxBytes);
    }
    if policy
        .max_age
        .is_some_and(|max| now > sealed && u128::from(now - sealed) > max.as_nanos())
    {
        out.push(Reason::MaxAge);
    }
    out
}

/// Applies a plan, revalidating the current checkpoint, segment identity, seal and oldest-end ordering.
///
/// Before each removal: the segment is the oldest in the journal and still the file planned (same identity), its
/// header and its successor's are valid, the successor is the next segment in the journal, and the segment is
/// strictly older than the checkpoint segment. Which segment is the oldest, and which is next, comes from a listing taken
/// when this starts and kept up to date with its own removals (ADR 0051 says why that is enough). The directory is listed
/// again, before anything else is removed, when a segment is missing, when a check fails, or when a segment removed
/// before is back (ADR 0051). Do not put segment files back into the journal while this runs. [`Action::Delete`] then checks the seal as [`plan`] does, without
/// reading the records it discards; [`Action::MoveTo`] reads the whole segment and validates every record and the seal
/// naming the successor, so that only a valid segment is moved.
///
/// Same-filesystem moves use an atomic no-replace rename. On `EXDEV`, a newly created destination copy is
/// synced, its directory synced, then the source removed and the journal directory synced. Partial copies stay
/// in the destination on error; they never justify removing the source. No existing destination is replaced.
///
/// The checks and the removal are separate steps, and this takes no lock, so a writer's recovery can run in between
/// (issue #83): with the marker lost and the segment damaged, a recovery without a marker starts at the oldest segment,
/// may replace it or end its chain in it, and the writer's next marker names it. So the removal takes the segment out of
/// the journal by a no-replace rename: a move into the destination; a deletion, and a move by copy once the copy is
/// durable, to a new temporary name in the journal directory, `{base:020}.tape.{pid}.{unix_ns}.removing`, that it then
/// unlinks. Before it lets the file go, it checks that it is the file checked before (same device and inode), that the
/// marker, read again and made durable, still names a later segment, and for a copy that the file still holds what was
/// copied. If not, it renames the file back, never replacing a file that has appeared under the segment's name, and
/// returns the error. A temporary name left behind by a crash is ignored by everything else, and reported in
/// [`Report::leftovers`]; it may hold records the journal no longer has, so a later run unlinks it only once retention
/// has removed the segment its seal names (see [`Report::leftovers`] and `docs/operations.md`).
///
/// Before the first removal that relies on a checkpoint marker slot, the marker file is synced (once per slot), so
/// that the slot is durable: a writer writes a slot before it syncs it, and a power loss in between must not bring back
/// an earlier marker naming a segment removed here (crash contract C14).
///
/// # Errors
///
/// [`Error::InvalidConfig`] for a plan used on a different/replaced journal, or lost eligibility;
/// [`Error::Corrupt`] / [`Error::UnsupportedVersion`] for invalid candidate data or checkpoint positions, including,
/// before the first removal, a marker whose position the records of its segment do not reach (as for [`plan`]);
/// [`Error::Io`] for I/O errors (including destination conflicts). Stops immediately, retaining earlier effects.
/// A missing source counts as already removed only when no older segment remains.
pub fn apply(dir: impl AsRef<Path>, plan: &Plan, action: &Action) -> Result<Report> {
    let mut report = Report::default();
    apply_into(
        dir.as_ref(),
        plan,
        action,
        &mut report,
        Known::default(),
        true,
        None,
        None,
    )?;
    Ok(report)
}

/// What a run knows of the oldest segments: their bases in order, from its last listing, without those it has removed
/// since. Empty: it has to list the directory.
///
/// Why this replaces a listing before each removal (ADR 0051). While a valid marker is in place, the bases before its
/// segment only disappear: a writer creates segments only after the newest one, a recovery from that marker only from
/// the marker's segment on, and archivers only remove. So a segment that was the oldest in a listing, or became it
/// because every older one is gone, is the oldest for as long as it exists, and no segment appears between it and the
/// one listed next. A recovery without a valid marker starts at the oldest segment and can replace and create segments
/// before the old marker's segment, but: no removal happens while the marker is invalid (it is read before each one); a
/// segment it replaced fails the identity check; and a segment it creates is the successor that a seal names, so it is
/// never in front of the oldest segment, and never between a segment and the successor that segment's seal names, which
/// every removal requires to be the planned one. Whether the segment and its successor still exist is checked by opening
/// both before each removal. Such a recovery between those checks and the removal is caught after the removal's rename
/// ([`Out::verify`]; issue #83).
#[derive(Debug, Default)]
struct Known(VecDeque<u64>);

impl Known {
    /// What the listing the plan was made from shows of its segments.
    fn from_plan(plan: &Plan) -> Known {
        let bases = plan.segments.iter().map(|s| s.base_seq);
        Known(
            bases
                .chain(plan.segments.last().map(|s| s.successor))
                .collect(),
        )
    }

    fn list(dir: &Path) -> Result<Known> {
        Ok(Known(
            journal::list_bases(dir)?.iter().map(|s| s.base).collect(),
        ))
    }

    /// `base` is gone, and everything older.
    fn already_removed(&self, base: u64) -> bool {
        self.0.front().is_some_and(|oldest| base < *oldest)
    }

    fn is_oldest(&self, base: u64) -> bool {
        self.0.front() == Some(&base)
    }

    fn is_next(&self, successor: u64) -> bool {
        self.0.get(1) == Some(&successor)
    }
}

/// `external` is true for a caller other than the writer itself (which runs retention only after its own marker is
/// durable): the marker that decides each removal is then made durable first ([`durable_checkpoint`]).
///
/// `stop` is given by the writer's retention run on its checkpoint thread: after the first planned segment, the run
/// ends before the next one once `stop(handled)` returns true for the number of planned segments handled so far (a
/// checkpoint is waiting and the run has removed what the checkpoint before it covered, or the writer is closing;
/// ADR 0044). The rest of the plan is left for the next run, which plans again.
///
/// `known` is what the caller already knows of the oldest segments, empty to list them first. `reached` is the marker
/// slot whose records the caller found reaching its position ([`reached_marker`]), if any.
#[allow(clippy::too_many_arguments)]
fn apply_into(
    dir: &Path,
    plan: &Plan,
    action: &Action,
    report: &mut Report,
    mut known: Known,
    external: bool,
    stop: Option<&dyn Fn(usize) -> bool>,
    mut reached: Option<CheckpointSlot>,
) -> Result<()> {
    if directory_identity(dir)? != plan.directory {
        return Err(Error::InvalidConfig(
            "retention plan belongs to a different journal directory".into(),
        ));
    }
    let mut synced = None;
    for (i, segment) in plan.segments.iter().enumerate() {
        if let Some(stop) = stop {
            if i > 0 && stop(i) {
                break;
            }
            crate::fault::hold_point(crate::fault::Point::RetentionRemove);
        }
        crate::fault::hook(crate::fault::Point::RetentionRemove);
        // A missing file is harmless only if the entire older prefix has also gone.
        let mut listed = false;
        if !known.already_removed(segment.base_seq)
            && (!known.is_oldest(segment.base_seq)
                || !known.is_next(segment.successor)
                || (i > 0 && is_back(&plan.segments[i - 1].path)))
        {
            known = Known::list(dir)?;
            listed = true;
        }
        let mut checked = check(
            dir,
            segment,
            action,
            external,
            (&mut synced, &mut reached),
            &known,
        );
        // What a check found is judged by a listing taken after it, unless one was taken just before it (but a missing
        // file is judged again even then: a concurrent archiver may have removed the segment in between). A marker that
        // could not be synced is reported as it is and nothing is tried after it: a later sync could succeed without
        // having written anything.
        if let Err(failed) = checked {
            checked = if failed.marker_io || (listed && !failed.is_missing()) {
                Err(failed)
            } else {
                known = Known::list(dir)?;
                failed.judged_by(&known, dir, segment)
            };
        }
        let Some(file) = checked.map_err(|failed| failed.error)? else {
            report.already_removed_segments = report.already_removed_segments.saturating_add(1);
            continue;
        };
        crate::fault::hook(crate::fault::Point::RetentionChecked);
        let mut removed = false;
        let guard = external.then_some(&mut synced);
        let result = remove(dir, &file, segment, action, &mut removed, guard);
        // Count the unlink/rename even if a subsequent sync fails.
        if removed {
            report.removed_segments = report.removed_segments.saturating_add(1);
            report.removed_bytes = report.removed_bytes.saturating_add(segment.size_bytes);
            known.0.pop_front();
        }
        if let Err(error) = result {
            if !removed && is_missing(&error) {
                known = Known::list(dir)?;
                if known.already_removed(segment.base_seq) {
                    report.already_removed_segments =
                        report.already_removed_segments.saturating_add(1);
                    continue;
                }
            }
            return Err(error);
        }
    }
    report.leftovers = remove_leftovers(dir, &plan.leftovers, &plan.listed);
    Ok(())
}

/// Whether a segment removed earlier in the run is there again (or whether it is cannot be told).
fn is_back(path: &Path) -> bool {
    crate::fault::note_access(crate::fault::Access::StatSegment);
    !matches!(fs::symlink_metadata(path), Err(e) if e.kind() == io::ErrorKind::NotFound)
}

fn is_missing(error: &Error) -> bool {
    matches!(error, Error::Io { source, .. } if source.kind() == io::ErrorKind::NotFound)
}

/// Whether `segment` is still there and the oldest one: false if it and everything older is gone (removed by a
/// concurrent archiver), an error if an older segment is there.
fn check_oldest(dir: &Path, known: &Known, segment: &PlannedSegment) -> Result<bool> {
    if known.already_removed(segment.base_seq) {
        return Ok(false);
    }
    if !known.is_oldest(segment.base_seq) {
        return Err(Error::corrupt(
            journal::segment_path(dir, segment.base_seq),
            0,
            CorruptReason::BrokenChain(segment.base_seq),
        ));
    }
    Ok(true)
}

/// A check that failed before a removal.
struct Failed {
    error: Error,
    /// The check of the successor's place or a later one.
    at_successor: bool,
    /// The marker could not be read or made durable.
    marker_io: bool,
}

/// What [`check`] found: the open file of the segment to remove, or `None` if it and everything older is gone.
type Checked = std::result::Result<Option<File>, Failed>;

impl Failed {
    fn new(error: Error) -> Failed {
        Failed {
            error,
            at_successor: false,
            marker_io: false,
        }
    }

    fn at_successor(error: Error) -> Failed {
        Failed {
            at_successor: true,
            ..Failed::new(error)
        }
    }

    fn is_missing(&self) -> bool {
        is_missing(&self.error)
    }

    /// The same failure in the light of a listing taken after it.
    fn judged_by(self, known: &Known, dir: &Path, segment: &PlannedSegment) -> Checked {
        if !check_oldest(dir, known, segment).map_err(Failed::new)? {
            return Ok(None);
        }
        if self.at_successor {
            check_next(dir, known, segment)?;
        }
        Err(self)
    }
}

/// Whether the successor is the next segment after this one.
fn check_next(
    dir: &Path,
    known: &Known,
    segment: &PlannedSegment,
) -> std::result::Result<(), Failed> {
    if known.is_next(segment.successor) {
        return Ok(());
    }
    Err(Failed::at_successor(Error::corrupt(
        journal::segment_path(dir, segment.base_seq),
        0,
        CorruptReason::BrokenChain(segment.successor),
    )))
}

/// Every check of a removal (crash contract C14): the segment is the oldest, the marker is valid, its segment's records
/// reach its position (ADR 0056; read once per slot, `reached`) and it names a later segment, the file is the one
/// planned, its header and the successor's are valid, the successor is next, and the seal or (for a move) every record
/// is valid. Removes nothing. `markers` are the slot last made durable ([`durable_checkpoint`]) and the slot last found
/// reaching its position.
fn check(
    dir: &Path,
    segment: &PlannedSegment,
    action: &Action,
    external: bool,
    markers: (&mut Option<CheckpointSlot>, &mut Option<CheckpointSlot>),
    known: &Known,
) -> Checked {
    if !check_oldest(dir, known, segment).map_err(Failed::new)? {
        return Ok(None);
    }
    let (synced, reached) = markers;
    let marker = if external {
        durable_reached_marker(dir, synced, reached).map_err(|error| Failed {
            marker_io: matches!(error, Error::Io { .. }),
            ..Failed::new(error)
        })?
    } else {
        reached_marker(
            dir,
            inspect::read_checkpoint(dir).map_err(Failed::new)?,
            reached,
        )
        .map_err(Failed::new)?
    };
    let Some(marker) = marker else {
        return Err(Failed::new(Error::InvalidConfig(
            "retention requires a valid checkpoint".into(),
        )));
    };
    if segment.base_seq >= marker.segment_base_seq {
        return Err(Failed::new(Error::InvalidConfig(
            "planned segment is no longer older than the checkpoint".into(),
        )));
    }
    let path = journal::segment_path(dir, segment.base_seq);
    let file = open_segment("open retention segment", &path).map_err(Failed::new)?;
    if journal::file_identity(&file, &path).map_err(Failed::new)? != segment.identity {
        return Err(Failed::new(Error::InvalidConfig(
            "planned segment has been replaced".into(),
        )));
    }
    let header = journal::read_header(&file, &path, segment.base_seq).map_err(Failed::new)?;
    // Check the expected successor before any destructive operation; a missing middle is never tolerated.
    check_next(dir, known, segment)?;
    let successor = journal::segment_path(dir, segment.successor);
    let next =
        open_segment("open retention successor", &successor).map_err(Failed::at_successor)?;
    journal::read_header(&next, &successor, segment.successor).map_err(Failed::at_successor)?;
    match action {
        Action::Delete => check_seal(
            &file,
            &path,
            header.segment_size,
            segment.base_seq,
            segment.successor,
        ),
        Action::MoveTo(_) => check_records(&file, &path, segment.base_seq, segment.successor),
    }
    .map_err(Failed::at_successor)?;
    Ok(Some(file))
}

/// Removes a segment that passed every check, and makes the removal durable. `removed` is set once its name is gone
/// from the journal, also if a later step fails, and cleared again if the segment is put back.
///
/// `guard` is given for an archiver other than the writer (the slot of the marker it last made durable): the segment's
/// name can then change between the checks and the removal, as a recovery without a valid marker may replace the
/// segment or end its chain in it (issue #83), so the removal acts on the file through a rename and then checks what it
/// took ([`Out`]). A writer's own run holds the writer lock, without which no recovery runs, and removes by name.
fn remove(
    dir: &Path,
    file: &File,
    segment: &PlannedSegment,
    action: &Action,
    removed: &mut bool,
    guard: Option<&mut Option<CheckpointSlot>>,
) -> Result<()> {
    let path = journal::segment_path(dir, segment.base_seq);
    let out = guard.map(|synced| Out {
        dir,
        segment,
        synced,
    });
    match (action, out) {
        (Action::Delete, None) => {
            fs::remove_file(&path).map_err(|e| Error::io("delete retention segment", &path, e))?;
            *removed = true;
        }
        (Action::Delete, Some(mut out)) => {
            let temp = take_out(dir, &path, segment.base_seq)?;
            *removed = true;
            out.verify(&temp, removed, &[dir], || Ok(()))?;
            remove_name(&temp)?;
        }
        (Action::MoveTo(dest), out) => move_segment(&path, dest, file, removed, out)?,
    }
    journal::sync_dir(dir)?;
    mmap::full_barrier(file).map_err(|e| Error::io("full sync retention segment", &path, e))?;
    // Its seek index, best effort (ADR 0045): a leftover is harmless, and the writer's next open removes it.
    crate::seek_index::dir::remove_index(dir, segment.base_seq);
    Ok(())
}

fn directory_identity(dir: &Path) -> Result<journal::Identity> {
    let meta = fs::metadata(dir).map_err(|e| Error::io("stat journal directory", dir, e))?;
    Ok((meta.dev(), meta.ino()))
}

/// [`checkpoint`], made durable before it is used: a writer writes a marker slot and syncs it afterwards (its marker
/// barrier), so a slot read in between can still be lost to a power loss, and removing the segments before it would
/// then leave the marker that comes back naming a removed segment. The marker file is synced after the read, which
/// makes durable every write the read saw. `synced` is the slot last made durable this way, which needs no new sync.
pub(crate) fn durable_checkpoint(
    dir: &Path,
    synced: &mut Option<CheckpointSlot>,
) -> Result<Option<CheckpointSlot>> {
    let Some((file, slot)) = read_marker(dir)? else {
        return Ok(None);
    };
    let slot = validate(dir, slot)?;
    make_durable(dir, &file, slot, synced)?;
    Ok(slot)
}

/// [`durable_checkpoint`] for a removal: the slot also passes [`reached_marker`] before it is made durable.
fn durable_reached_marker(
    dir: &Path,
    synced: &mut Option<CheckpointSlot>,
    reached: &mut Option<CheckpointSlot>,
) -> Result<Option<CheckpointSlot>> {
    let Some((file, slot)) = read_marker(dir)? else {
        return Ok(None);
    };
    let slot = reached_marker(dir, slot, reached)?;
    make_durable(dir, &file, slot, synced)?;
    Ok(slot)
}

/// The current slot of the marker, made durable as in [`durable_checkpoint`], without checking its segment.
fn durable_slot(dir: &Path, synced: &mut Option<CheckpointSlot>) -> Result<Option<CheckpointSlot>> {
    let Some((file, slot)) = read_marker(dir)? else {
        return Ok(None);
    };
    make_durable(dir, &file, slot, synced)?;
    Ok(slot)
}

/// The marker file and its current slot ([`inspect::read_checkpoint`]), or `None` if there is no marker file.
fn read_marker(dir: &Path) -> Result<Option<(File, Option<CheckpointSlot>)>> {
    let path = dir.join(journal::CHECKPOINT_FILE);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io("open checkpoint", &path, e)),
    };
    let slot = inspect::read_checkpoint_from(&file, &path)?;
    Ok(Some((file, slot)))
}

/// Syncs the marker file (open as `file`) unless `slot` is none or the one made durable last.
fn make_durable(
    dir: &Path,
    file: &File,
    slot: Option<CheckpointSlot>,
    synced: &mut Option<CheckpointSlot>,
) -> Result<()> {
    if slot.is_some() && slot != *synced {
        crate::fault::inject(crate::fault::Point::RetentionMarkerSync)
            .and_then(|()| mmap::full_barrier(file))
            .map_err(|e| {
                Error::io(
                    "sync checkpoint marker",
                    dir.join(journal::CHECKPOINT_FILE),
                    e,
                )
            })?;
        *synced = slot;
    }
    Ok(())
}

/// Validate the position to the same extent as writer recovery, without scanning the mutable active tail.
pub(crate) fn checkpoint(dir: &Path) -> Result<Option<CheckpointSlot>> {
    validate(dir, inspect::read_checkpoint(dir)?)
}

/// [`checkpoint`] for a slot already read.
fn validate(dir: &Path, slot: Option<CheckpointSlot>) -> Result<Option<CheckpointSlot>> {
    Ok(open_marker_segment(dir, slot)?.map(|(slot, ..)| slot))
}

/// [`validate`] for retention and archiving: the slot also passes the writer's check that the records of its segment
/// reach its position (`format.md` → Recovery, step 2; ADR 0056), or the run fails with the error
/// [`Writer::open`](crate::Writer::open) gives for it, `DanglingCheckpoint` at the segment and the marker's offset, and
/// removes nothing: the remedies after such a refusal (restore the matching marker, or the segment, from a copy;
/// `docs/operations.md` → After a crash) need the segments retention would remove. The check reads the record right
/// before the marker's offset (`journal::marker_reached`), once per slot: not again while `reached` is that slot.
fn reached_marker(
    dir: &Path,
    slot: Option<CheckpointSlot>,
    reached: &mut Option<CheckpointSlot>,
) -> Result<Option<CheckpointSlot>> {
    let Some((slot, file, size)) = open_marker_segment(dir, slot)? else {
        return Ok(None);
    };
    if *reached != Some(slot) {
        let path = journal::segment_path(dir, slot.segment_base_seq);
        if !journal::marker_reached(&file, &path, size, &slot)? {
            return Err(Error::corrupt(
                path,
                slot.offset,
                CorruptReason::DanglingCheckpoint(slot.segment_base_seq),
            ));
        }
        *reached = Some(slot);
    }
    Ok(Some(slot))
}

/// [`validate`], with the marker's segment open and its size from its header.
fn open_marker_segment(
    dir: &Path,
    slot: Option<CheckpointSlot>,
) -> Result<Option<(CheckpointSlot, File, u64)>> {
    let Some(slot) = slot else {
        return Ok(None);
    };
    crate::fault::hook(crate::fault::Point::MarkerRead);
    let path = journal::segment_path(dir, slot.segment_base_seq);
    crate::fault::note_access(crate::fault::Access::OpenSegment);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(Error::corrupt(
                dir.join(journal::CHECKPOINT_FILE),
                0,
                CorruptReason::DanglingCheckpoint(slot.segment_base_seq),
            ));
        }
        Err(e) => return Err(Error::io("open checkpoint segment", &path, e)),
    };
    let header = journal::read_header(&file, &path, slot.segment_base_seq)?;
    if slot.offset < format::SEGMENT_HEADER_LEN as u64
        || slot.offset > header.segment_size - format::SEAL_LEN as u64
        || slot.offset % 8 != 0
    {
        return Err(Error::corrupt(
            dir.join(journal::CHECKPOINT_FILE),
            0,
            CorruptReason::DanglingCheckpoint(slot.segment_base_seq),
        ));
    }
    Ok(Some((slot, file, header.segment_size)))
}

/// The first read of [`seal_at_end`], in bytes; each further read doubles, up to [`TAIL_READ_MAX`].
const TAIL_READ: u64 = 4096;
/// The largest read of [`seal_at_end`].
const TAIL_READ_MAX: u64 = 1 << 20;

/// Checks that segment `base` (open as `file`, `size` bytes, header already validated) is sealed and its seal names
/// `successor`, reading only the end of the file back to the seal ([`seal_at_end`]). If the end is not such a seal,
/// it falls back to [`check_records`], which reads the whole segment and reports exactly what is wrong (or accepts a
/// valid chain of records ending in a valid seal that has non-zero bytes after it).
fn check_seal(file: &File, path: &Path, size: u64, base: u64, successor: u64) -> Result<()> {
    if seal_at_end(file, path, size, successor)? {
        return Ok(());
    }
    check_records(file, path, base, successor)
}

/// Whether the last non-zero eight bytes of the record area of a segment of `size` bytes are a valid seal for
/// `successor` (`docs/format.md` → Seal) with room for at least one record before it. A seal is eight bytes at an 8-byte-aligned
/// offset with only zeros after it, so the 8-byte word holding the last non-zero byte is the seal. Reads backwards
/// from the end of the file, 4 KiB first and twice as much each time up to 1 MiB, so it reads the zeros after the seal
/// and the seal (normally a single read): what was left free when the segment rolled, less than the frame that did
/// not fit, plus the trailer.
fn seal_at_end(file: &File, path: &Path, size: u64, successor: u64) -> Result<bool> {
    Ok(seal_crc_at_end(file, path, size)? == Some(format::seal_crc(successor)))
}

/// The CRC word of the seal that is the last non-zero 8-byte word of the record area of a segment of `size` bytes (a
/// multiple of 64 KiB), with room for at least one record before it, or `None` if that word is no seal. Reads as
/// [`seal_at_end`] says.
fn seal_crc_at_end(file: &File, path: &Path, size: u64) -> Result<Option<u32>> {
    let first_slot = format::SEGMENT_HEADER_LEN as u64;
    let mut end = size;
    let mut chunk = TAIL_READ;
    let mut buffer = Vec::new();
    while end > first_slot {
        // `size` is a multiple of 64 KiB and `chunk` of 4 KiB, so every read starts and ends 8-byte aligned.
        let start = end.saturating_sub(chunk).max(first_slot);
        buffer.resize((end - start) as usize, 0);
        file.read_exact_at(&mut buffer, start)
            .map_err(|e| Error::io("read segment end", path, e))?;
        if let Some(word) = buffer.chunks_exact(8).rposition(|w| w != [0; 8]) {
            let at = start + word as u64 * 8;
            let bytes = &buffer[word * 8..word * 8 + 8];
            let commit = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            let crc = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
            // The earliest seal follows one record with an empty payload.
            let sealed = at >= first_slot + format::RECORD_HEADER_LEN as u64
                && commit == format::SEAL_MARKER;
            return Ok(sealed.then_some(crc));
        }
        end = start;
        chunk = (chunk * 2).min(TAIL_READ_MAX);
    }
    Ok(None)
}

/// Reads segment `base` (open as `file`) from its first record and validates every record and the seal, which must
/// name `successor`.
fn check_records(file: &File, path: &Path, base: u64, successor: u64) -> Result<()> {
    // Bounded memory: segments can be up to 2^40 bytes, and this runs on the writer's threads.
    let scan = journal::scan_open(file, path, base)?;
    match scan.end {
        ScanEnd::Seal if scan.next_seq == successor => Ok(()),
        ScanEnd::Invalid(reason) => Err(Error::corrupt(path, scan.end_offset, reason)),
        _ => Err(Error::corrupt(
            path,
            scan.end_offset,
            CorruptReason::BrokenChain(scan.next_seq),
        )),
    }
}

fn move_segment(
    path: &Path,
    dest: &Path,
    source: &File,
    removed: &mut bool,
    out: Option<Out<'_>>,
) -> Result<()> {
    let target = dest.join(
        path.file_name()
            .ok_or_else(|| Error::InvalidConfig("segment has no basename".into()))?,
    );
    // Safe OS wrapper: RENAME_NOREPLACE on Linux, RENAME_EXCL on macOS. No check-then-rename race.
    let renamed = crate::fault::inject(crate::fault::Point::RetentionRename).and_then(|()| {
        rustix::fs::renameat_with(
            rustix::fs::CWD,
            path,
            rustix::fs::CWD,
            &target,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(io::Error::from)
    });
    match renamed {
        Ok(()) => {
            *removed = true;
            if let Some(mut out) = out {
                out.verify(&target, removed, &[out.dir, dest], || Ok(()))?;
            }
            sync_destination(dest)?;
        }
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            let mut copy = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map_err(|e| Error::io("create archive copy", &target, e))?;
            // pread leaves the source cursor untouched (the same File is also used for barriers).
            use std::io::Write;
            let copied = read_through(source, path, |bytes| {
                copy.write_all(bytes)
                    .map_err(|e| Error::io("write archive copy", &target, e))
            })?;
            crate::fault::hook(crate::fault::Point::RetentionCopySync);
            crate::fault::inject(crate::fault::Point::RetentionCopySync)
                .map_err(|e| Error::io("sync archive copy", &target, e))?;
            mmap::sync_all(&copy).map_err(|e| Error::io("sync archive copy", &target, e))?;
            sync_destination(dest)?;
            mmap::full_barrier(&copy)
                .map_err(|e| Error::io("full sync archive copy", &target, e))?;
            match out {
                None => {
                    fs::remove_file(path)
                        .map_err(|e| Error::io("remove archived segment", path, e))?;
                    *removed = true;
                }
                Some(mut out) => {
                    let temp = take_out(out.dir, path, out.segment.base_seq)?;
                    *removed = true;
                    // The copy was read from the file checked, but by name the segment could still be found and
                    // written to until it was taken out (a recovery that ended its chain in it, and a writer that
                    // then filled it): once the checks pass nothing writes to it any more, and the copy must hold
                    // what it holds then.
                    out.verify(&temp, removed, &[out.dir], || {
                        if read_through(source, path, |_| Ok(()))? == copied {
                            Ok(())
                        } else {
                            Err(Error::InvalidConfig(
                                "planned segment changed while it was copied".into(),
                            ))
                        }
                    })?;
                    remove_name(&temp)?;
                }
            }
        }
        Err(e) => return Err(Error::io("rename archive segment", &target, e)),
    }
    Ok(())
}

fn sync_destination(dest: &Path) -> Result<()> {
    crate::fault::inject(crate::fault::Point::RetentionDestinationSync)
        .map_err(|e| Error::io("sync archive directory", dest, e))?;
    journal::sync_dir(dest)
}

/// Reads `file` (at `path`, for errors) from its start to its end with `pread`, passing each piece to `each`: its length
/// and CRC32C.
fn read_through(
    file: &File,
    path: &Path,
    mut each: impl FnMut(&[u8]) -> Result<()>,
) -> Result<(u64, u32)> {
    let (mut offset, mut crc) = (0, 0);
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let count = file
            .read_at(&mut buffer, offset)
            .map_err(|e| Error::io("read archive source", path, e))?;
        if count == 0 {
            return Ok((offset, crc));
        }
        each(&buffer[..count])?;
        crc = crate::crc::crc32c_append(crc, &buffer[..count]);
        offset += count as u64;
    }
}

// Removals by an archiver other than the writer (issue #83). Its checks of a segment and the removal are separate steps,
// and without the writer lock a recovery can run in between: with the marker lost and the segment damaged, a recovery
// without a valid marker starts at the oldest segment, may end its chain there, and the writer's next marker names
// that segment, which by name is either a replacement (a new file) or the same file, written to since. So the removal
// takes the segment out of the journal by a rename (into the destination, or to a temporary name in the journal
// directory that is then unlinked), and checks what it took before it lets it go.

/// Suffix of the temporary name of a segment being removed: `{base:020}.tape.{pid}.{unix_ns}.removing`
/// (`docs/format.md` → Journal directory). It does not parse as a segment's name, nor as a writer's temporary file
/// (`*.tape.tmp`), so the writer, its recovery, readers, `verify` and `status` (tickertape 1.0.0 included) ignore it.
const REMOVING: &str = ".removing";

/// How old a leftover temporary name must be before a retention run unlinks it ([`remove_leftovers`]).
const LEFTOVER_AGE_NS: u64 = 3_600_000_000_000;

/// A temporary name of a segment being removed, as a listing shows it ([`REMOVING`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Leftover {
    name: String,
    base: u64,
    pid: u32,
    unix_ns: u64,
}

impl Leftover {
    fn name(base: u64, unix_ns: u64) -> String {
        let segment = format::segment_file_name(base);
        format!("{segment}.{}.{unix_ns}{REMOVING}", std::process::id())
    }

    fn parse(name: &str) -> Option<Leftover> {
        let rest = name.strip_suffix(REMOVING)?;
        let (segment, rest) = rest.split_at_checked(25)?;
        let base = format::parse_segment_file_name(segment)?;
        let (pid, unix_ns) = rest.strip_prefix('.')?.split_once('.')?;
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if !digits(pid) || !digits(unix_ns) {
            return None;
        }
        Some(Leftover {
            name: name.to_owned(),
            base,
            pid: pid.parse().ok()?,
            unix_ns: unix_ns.parse().ok()?,
        })
    }
}

/// Whether `name` is the temporary name of a segment being removed ([`REMOVING`]).
pub(crate) fn is_leftover_name(name: &str) -> bool {
    Leftover::parse(name).is_some()
}

/// `rename(from, to)` that never replaces a file at `to`: `renameat2(RENAME_NOREPLACE)` on Linux, `renameatx_np`
/// with `RENAME_EXCL` on macOS.
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        from,
        rustix::fs::CWD,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(io::Error::from)
}

/// The file system has no no-replace rename.
fn no_replace_unsupported(e: &io::Error) -> bool {
    e.raw_os_error()
        .is_some_and(|e| [libc::EINVAL, libc::ENOTSUP, libc::EOPNOTSUPP, libc::ENOSYS].contains(&e))
}

/// Renames the segment at `path` to a new temporary name in `dir` ([`REMOVING`]) and returns that name. Nothing is
/// replaced: a name that is taken is skipped. A missing segment is the error "not found", as for an unlink.
fn take_out(dir: &Path, path: &Path, base: u64) -> Result<PathBuf> {
    let start = crate::fault::temporary_name_ns().unwrap_or_else(crate::clock::realtime_ns);
    for unix_ns in (start..).take(64) {
        let temp = dir.join(Leftover::name(base, unix_ns));
        let renamed = crate::fault::inject(crate::fault::Point::RetentionTakeOut)
            .and_then(|()| rename_no_replace(path, &temp));
        match renamed {
            Ok(()) => return Ok(temp),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            // The name is this process's own and new: only a leftover of an earlier process with the same id can
            // have it, and nothing else creates it meanwhile, so a plain rename replaces nothing if it is free.
            Err(e) if no_replace_unsupported(&e) => match fs::symlink_metadata(&temp) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    fs::rename(path, &temp)
                        .map_err(|e| Error::io("take out retention segment", path, e))?;
                    return Ok(temp);
                }
                Err(e) => return Err(Error::io("take out retention segment", &temp, e)),
                Ok(_) => {}
            },
            Err(e) => return Err(Error::io("take out retention segment", path, e)),
        }
    }
    Err(Error::io(
        "take out retention segment",
        path,
        io::Error::from(io::ErrorKind::AlreadyExists),
    ))
}

/// Renames a segment taken out of the journal (at `at`) back to `path` without replacing a file there, and syncs
/// `dirs`. Without a no-replace rename it links `path` to the file (`link` never replaces either) and unlinks `at`, a
/// name only this removal uses. If that unlink fails, the segment is back but `at` stays a second link to it (a
/// writer writing to the segment writes through it too), and that is the error.
fn put_back(at: &Path, path: &Path, dirs: &[&Path]) -> Result<()> {
    crate::fault::hook(crate::fault::Point::RetentionPutBack);
    let renamed = crate::fault::inject(crate::fault::Point::RetentionPutBack)
        .and_then(|()| rename_no_replace(at, path));
    match renamed {
        Ok(()) => {}
        Err(e) if no_replace_unsupported(&e) => {
            fs::hard_link(at, path).map_err(|e| Error::io("put back retention segment", at, e))?;
            crate::fault::inject(crate::fault::Point::RetentionPutBackUnlink)
                .and_then(|()| fs::remove_file(at))
                .map_err(|e| {
                    Error::io(
                        "unlink a second link to a segment put back (it stays a second name of the segment)",
                        at,
                        e,
                    )
                })?;
        }
        Err(e) => return Err(Error::io("put back retention segment", at, e)),
    }
    for dir in dirs {
        journal::sync_dir(dir)?;
    }
    Ok(())
}

/// Unlinks a temporary name whose segment has been checked (or copied and checked).
fn remove_name(temp: &Path) -> Result<()> {
    fs::remove_file(temp).map_err(|e| Error::io("delete retention segment", temp, e))
}

/// What a removal by an archiver other than the writer checks once the segment is out of the journal.
struct Out<'a> {
    dir: &'a Path,
    segment: &'a PlannedSegment,
    /// The marker slot last made durable ([`durable_checkpoint`]).
    synced: &'a mut Option<CheckpointSlot>,
}

impl Out<'_> {
    /// Checks the segment taken out of the journal to `at`, and `more`. If a check fails, or cannot be made, it puts
    /// the segment back (clearing `removed`, syncing `dirs`) and returns why; if it cannot put it back, the error says
    /// where the file is.
    ///
    /// The checks: the file at `at` is the one checked before (same device and inode: the run holds that file open, so
    /// its inode number cannot have gone to another file), and the marker, read now and made durable, is valid and names
    /// a later segment. With the segment out of the journal by name, no recovery can find it and no writer can start
    /// writing to it, so a valid marker naming a later segment now means that no marker will name it again: a writer's
    /// markers only move forward, and a later recovery starts from that marker or, without one, from a segment that is
    /// in the journal. If the marker names this segment, or none, a writer may be writing to it, and it goes back.
    fn verify(
        &mut self,
        at: &Path,
        removed: &mut bool,
        dirs: &[&Path],
        more: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        crate::fault::hook(crate::fault::Point::RetentionTakenOut);
        crate::fault::note_access(crate::fault::Access::StatSegment);
        let failed = match journal::path_identity(at) {
            // Gone: not this run's to put back (nothing removes a temporary name this young).
            Ok(None) => {
                return Err(Error::io(
                    "check removed segment",
                    at,
                    io::Error::from(io::ErrorKind::NotFound),
                ));
            }
            Ok(Some(found)) if found != self.segment.identity => Err(Error::InvalidConfig(
                "planned segment has been replaced".into(),
            )),
            Ok(Some(_)) => self.marker_past().and_then(|()| more()),
            Err(e) => Err(e),
        };
        let Err(failed) = failed else {
            crate::fault::hook(crate::fault::Point::RetentionVerified);
            return Ok(());
        };
        let path = journal::segment_path(self.dir, self.segment.base_seq);
        let put = put_back(at, &path, dirs);
        // Not removed: put back, or, if that failed, still under `at`, out of the journal but not deleted or moved.
        *removed = false;
        match put {
            Ok(()) => Err(failed),
            // Both reasons. Never "not found", which a caller would take for another archiver's removal.
            Err(error) => Err(Error::io(
                "put back retention segment",
                at,
                io::Error::other(format!("{error}; the check that failed: {failed}")),
            )),
        }
    }

    fn marker_past(&mut self) -> Result<()> {
        match durable_slot(self.dir, self.synced)? {
            None => Err(Error::InvalidConfig(
                "retention requires a valid checkpoint".into(),
            )),
            Some(slot) if slot.segment_base_seq <= self.segment.base_seq => {
                Err(Error::InvalidConfig(
                    "planned segment is no longer older than the checkpoint".into(),
                ))
            }
            Some(_) => Ok(()),
        }
    }
}

/// Unlinks the temporary names of segments that interrupted removals left behind ([`REMOVING`]), best effort, and
/// returns those it leaves.
///
/// Such a file may hold records the journal no longer has: a crash between taking a segment out and putting it back,
/// while a writer that already held the segment filled it and moved its marker past it, leaves the only copy of
/// covered records there (ADR 0055), and nothing names that file. So a name goes only when the file is sealed and the
/// segment its seal names is gone from the journal, below its oldest segment (`listed`, the plan's listing): newer
/// records have then been removed on purpose, by retention, and removing this file is what retention would have done.
/// An unsealed file, or one whose seal names a segment still listed or another leftover (kept, or going in this run:
/// it is not something retention removed), or a missing segment above the oldest, stays and is reported. As an extra guard, a name counts only once it is [`LEFTOVER_AGE_NS`] old and its process is gone,
/// so that a removal in progress is never touched. Nothing is ever put back: a segment put in front of the oldest one
/// could sit in front of a gap (crash contract C14).
///
/// Which segment a seal names is told from its CRC word: compared with the seal of each listed base, which costs the
/// end of the file only; when none matches, the whole file is scanned, and its records validated, for the sequence
/// number after its last record.
fn remove_leftovers(dir: &Path, leftovers: &[Leftover], listed: &[u64]) -> Vec<PathBuf> {
    let now = crate::clock::realtime_ns();
    let present: Vec<u64> = listed
        .iter()
        .copied()
        .chain(leftovers.iter().map(|l| l.base))
        .collect();
    let mut left = Vec::new();
    for leftover in leftovers {
        let path = dir.join(&leftover.name);
        let goes = now.saturating_sub(leftover.unix_ns) >= LEFTOVER_AGE_NS
            && !process_alive(leftover.pid)
            && successor_gone(&path, leftover.base, listed, &present).unwrap_or(false);
        if goes {
            match fs::remove_file(&path) {
                Ok(()) => continue,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => {}
            }
        }
        if path.exists() {
            left.push(path);
        }
    }
    left
}

/// Whether the file at `path`, a segment taken out of the journal with base `base`, is sealed and its seal names a
/// segment below the oldest in `listed` (see [`remove_leftovers`]).
///
/// `present` is what counts as still there: the listed segments and the bases of all leftovers (the segment a seal names
/// may itself be a kept leftover, which retention did not remove).
fn successor_gone(path: &Path, base: u64, listed: &[u64], present: &[u64]) -> Result<bool> {
    let Some(&oldest) = listed.first() else {
        return Ok(false);
    };
    let file = File::open(path).map_err(|e| Error::io("open removed segment", path, e))?;
    let size = file
        .metadata()
        .map_err(|e| Error::io("stat removed segment", path, e))?
        .len();
    if format::validate_segment_size(size).is_err() {
        return Ok(false);
    }
    let Some(crc) = seal_crc_at_end(&file, path, size)? else {
        return Ok(false);
    };
    if present
        .iter()
        .any(|b| *b > base && format::seal_crc(*b) == crc)
    {
        return Ok(false);
    }
    journal::read_header(&file, path, base)?;
    let scan = journal::scan_open(&file, path, base)?;
    Ok(matches!(scan.end, ScanEnd::Seal)
        && format::seal_crc(scan.next_seq) == crc
        && scan.next_seq < oldest)
}

/// Whether process `pid` may be running (`kill(pid, 0)`); true when it cannot be told.
fn process_alive(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return true;
    };
    !matches!(rustix::process::test_kill_process(pid), Err(e) if e == rustix::io::Errno::SRCH)
}

#[cfg(test)]
mod planning_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_boundary_future_and_or_limits() {
        let p = Policy::default().max_age(Duration::from_nanos(10));
        assert!(reasons(&p, 4, 100, 110, 100).is_empty());
        assert_eq!(reasons(&p, 4, 100, 111, 100), [Reason::MaxAge]);
        assert!(reasons(&p, 4, 100, 99, 100).is_empty());
        assert_eq!(
            reasons(&p.max_segments(3).max_bytes(0), 4, 100, 111, 100),
            [Reason::MaxSegments, Reason::MaxBytes, Reason::MaxAge]
        );
    }

    /// The temporary name of a segment being removed: what it parses to, and that it is no other name with a meaning
    /// (`docs/format.md` → Journal directory).
    #[test]
    fn leftover_names() {
        let name = Leftover::name(1 << 20, 1_700_000_000_123_456_789);
        let parsed = Leftover::parse(&name).unwrap();
        assert_eq!(
            (parsed.base, parsed.pid, parsed.unix_ns),
            (1 << 20, std::process::id(), 1_700_000_000_123_456_789)
        );
        assert_eq!(format::parse_segment_file_name(&name), None);
        assert!(!name.ends_with(journal::TMP_SUFFIX) && name != journal::SPARE_FILE);
        let segment = format::segment_file_name(5);
        for other in [
            format!("{segment}.removing"),
            format!("{segment}.1.removing"),
            format!("{segment}.1.2.3.removing"),
            format!("{segment}.1.x.removing"),
            format!("{segment}..2.removing"),
            format!("{segment}.+1.2.removing"),
            format!("{segment}.1.2.removing.tmp"),
            format!("{segment}x1.2.removing"),
            "0000000000000000000x.tape.1.2.removing".to_owned(),
            format!("{segment}.99999999999.2.removing"),
            "é".repeat(13) + ".1.2.removing",
        ] {
            assert_eq!(Leftover::parse(&other), None, "{other}");
        }
    }

    /// A file of `size` zero bytes with `words` (offset, eight bytes) written into it.
    fn image(size: u64, words: &[(u64, [u8; 8])]) -> (tempfile::TempDir, File) {
        let dir = tempfile::tempdir().unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.path().join("segment"))
            .unwrap();
        file.set_len(size).unwrap();
        for (offset, word) in words {
            file.write_all_at(word, *offset).unwrap();
        }
        (dir, file)
    }

    fn seal(successor: u64) -> [u8; 8] {
        let mut word = [0; 8];
        word[..4].copy_from_slice(&format::SEAL_MARKER.to_le_bytes());
        word[4..].copy_from_slice(&format::seal_crc(successor).to_le_bytes());
        word
    }

    /// The edges of [`seal_at_end`]: where a seal can be, what counts as the last non-zero word, and which successor
    /// it must name. (Whether the records before it are valid is not its business: `tests/retention.rs`.)
    #[test]
    fn seal_at_end_accepts_only_a_seal_for_the_successor_as_the_last_non_zero_word() {
        const SIZE: u64 = 4 << 20;
        let first = format::SEGMENT_HEADER_LEN as u64;
        let earliest = first + format::RECORD_HEADER_LEN as u64;
        let found = |words: &[(u64, [u8; 8])]| {
            let (dir, file) = image(SIZE, words);
            seal_at_end(&file, &dir.path().join("segment"), SIZE, 7).unwrap()
        };
        // In the last word of the file, in the first read; further back, after several doubling reads (more than
        // 1 MiB of zeros behind it); and at the earliest offset a seal can have.
        assert!(found(&[(SIZE - 8, seal(7))]));
        assert!(found(&[(SIZE - (2 << 20), seal(7))]));
        assert!(found(&[(earliest, seal(7))]));
        // Earlier content does not matter, valid or not.
        assert!(found(&[(first, [0xAB; 8]), (4096, seal(7))]));
        // No room for a record before it: the first slot, and the words up to the earliest seal.
        for at in (first..earliest).step_by(8) {
            assert!(!found(&[(at, seal(7))]), "seal at {at}");
        }
        // Nothing but zeros after the header; a seal for another successor; a damaged seal.
        assert!(!found(&[]));
        assert!(!found(&[(4096, seal(8))]));
        let mut damaged = seal(7);
        damaged[7] ^= 1;
        assert!(!found(&[(4096, damaged)]));
        // Anything non-zero after a valid seal, even one byte in the last word of the file, is not this shape (the
        // caller then reads the whole segment).
        assert!(!found(&[(4096, seal(7)), (4104, [0, 0, 0, 0, 0, 0, 0, 1])]));
        assert!(!found(&[
            (4096, seal(7)),
            (SIZE - 8, [1, 0, 0, 0, 0, 0, 0, 0])
        ]));
        // A later seal for the successor is the one that counts.
        assert!(found(&[(4096, seal(8)), (8192, seal(7))]));
    }
}
