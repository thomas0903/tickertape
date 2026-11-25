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
}

impl Report {
    /// Adds another report's counts to this one (saturating).
    pub(crate) fn absorb(&mut self, other: &Report) {
        self.removed_segments = self.removed_segments.saturating_add(other.removed_segments);
        self.removed_bytes = self.removed_bytes.saturating_add(other.removed_bytes);
        self.already_removed_segments = self
            .already_removed_segments
            .saturating_add(other.already_removed_segments);
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
/// - [`Policy::max_bytes`]: the size of every segment file (its metadata, the file is not opened).
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
/// successor, to the marker's segment, or (with `max_age`) to the first segment that is young enough or its successor.
/// Other segments are not opened, and damage in them does not stop retention (ADR 0051): run
/// [`inspect::verify`](crate::inspect::verify) to find it.
///
/// # Errors
///
/// [`Error::NotFound`] for an empty journal; [`Error::Corrupt`] or [`Error::UnsupportedVersion`] for an invalid
/// header, checkpoint position, or candidate seal or chain; [`Error::Io`] for filesystem errors, including concurrent
/// changes while observing the directory. An absent/invalid checkpoint slot yields an empty plan.
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
/// A file the listing no longer shows is forgotten, whoever removed it. That is enough because a segment file is never
/// resized (`docs/format.md` → Segment file) and only a recovery replaces one, which needs the lock the writer holds. The
/// sizes only decide which segments a limit asks to remove; nothing that protects a segment from removal uses them.
#[derive(Debug, Default)]
pub(crate) struct Sizes {
    /// (base, inode, size), by base.
    known: Vec<(u64, u64, u64)>,
}

impl Sizes {
    /// The size of each listed segment file.
    fn of(&mut self, dir: &Path, listed: &[Listed]) -> Result<Vec<u64>> {
        let mut known = std::mem::take(&mut self.known).into_iter().peekable();
        let mut sizes = Vec::with_capacity(listed.len());
        let mut keep = Vec::with_capacity(listed.len());
        for segment in listed {
            while known.next_if(|(base, _, _)| *base < segment.base).is_some() {}
            let seen = known.next_if(|(base, _, _)| *base == segment.base);
            let size = match (seen, segment.inode) {
                (Some((_, inode, size)), Some(now)) if inode == now => size,
                _ => {
                    let path = journal::segment_path(dir, segment.base);
                    crate::fault::note_access(crate::fault::Access::StatSegment);
                    fs::metadata(&path)
                        .map_err(|e| Error::io("stat segment", &path, e))?
                        .len()
                }
            };
            sizes.push(size);
            if let Some(inode) = segment.inode {
                keep.push((segment.base, inode, size));
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
    let plan = plan_at(dir, policy, now, stop.unwrap_or(&|_| false), sizes)?;
    let known = Known::from_plan(&plan);
    apply_into(dir, &plan, action, report, known, false, stop)
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
    let directory = directory_identity(dir)?;
    let listed = journal::list_bases(dir)?;
    if listed.is_empty() {
        return Err(Error::NotFound { dir: dir.into() });
    }
    let mut plan = Plan {
        segments: Vec::new(),
        directory,
    };
    if policy == &Policy::default() {
        return Ok(plan);
    }
    let Some(checkpoint) = checkpoint(dir)? else {
        return Ok(plan);
    };
    let mut count = listed.len();
    let sizes = match policy.max_bytes {
        Some(_) => sizes.of(dir, &listed)?,
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
/// when this starts and kept up to date: while a run removes segments, names older than the checkpoint segment only
/// disappear (no writer, recovery or archiver creates one there). The directory is listed again, before anything else is
/// removed, when a segment is missing, when a check fails, or when a segment removed before is back (ADR 0051). Do not
/// put segment files back into the journal while this runs. [`Action::Delete`] then checks the seal as [`plan`] does, without
/// reading the records it discards; [`Action::MoveTo`] reads the whole segment and validates every record and the seal
/// naming the successor, so that only a valid segment is moved.
///
/// Same-filesystem moves use an atomic no-replace rename. On `EXDEV`, a newly created destination copy is
/// synced, its directory synced, then the source removed and the journal directory synced. Partial copies stay
/// in the destination on error; they never justify removing the source. No existing destination is replaced.
///
/// Before the first removal that relies on a checkpoint marker slot, the marker file is synced (once per slot), so
/// that the slot is durable: a writer writes a slot before it syncs it, and a power loss in between must not bring back
/// an earlier marker naming a segment removed here (crash contract C14).
///
/// # Errors
///
/// [`Error::InvalidConfig`] for a plan used on a different/replaced journal, or lost eligibility;
/// [`Error::Corrupt`] / [`Error::UnsupportedVersion`] for invalid candidate data or checkpoint positions;
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
    )?;
    Ok(report)
}

/// What a run knows of the oldest segments: their bases in order, from its last listing, without those it has removed
/// since. Empty: it has to list the directory.
///
/// Why this replaces a listing before each removal (ADR 0051): the bases before the marker's segment only disappear
/// while the run goes on. A writer creates segments only after the newest one, a recovery only in what it recovers
/// (from the marker's segment on), and archivers only remove. So a segment that was the oldest in a listing, or became
/// it because every older one is gone, is the oldest for as long as it exists, and no segment appears between it and
/// the one listed next. Whether the two still exist is checked by opening both before each removal.
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
/// `known` is what the caller already knows of the oldest segments, empty to list them first.
fn apply_into(
    dir: &Path,
    plan: &Plan,
    action: &Action,
    report: &mut Report,
    mut known: Known,
    external: bool,
    stop: Option<&dyn Fn(usize) -> bool>,
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
        let mut checked = check(dir, segment, action, external, &mut synced, &known);
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
        let result = remove(dir, &file, segment, action, &mut removed);
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

/// Every check of a removal (crash contract C14): the segment is the oldest, the marker is valid and names a later one,
/// the file is the one planned, its header and the successor's are valid, the successor is next, and the seal or (for a
/// move) every record is valid. Removes nothing.
fn check(
    dir: &Path,
    segment: &PlannedSegment,
    action: &Action,
    external: bool,
    synced: &mut Option<CheckpointSlot>,
    known: &Known,
) -> Checked {
    if !check_oldest(dir, known, segment).map_err(Failed::new)? {
        return Ok(None);
    }
    let marker = if external {
        durable_checkpoint(dir, synced).map_err(|error| Failed {
            marker_io: matches!(error, Error::Io { .. }),
            ..Failed::new(error)
        })?
    } else {
        checkpoint(dir).map_err(Failed::new)?
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
/// from the journal, also if a later step fails.
fn remove(
    dir: &Path,
    file: &File,
    segment: &PlannedSegment,
    action: &Action,
    removed: &mut bool,
) -> Result<()> {
    let path = journal::segment_path(dir, segment.base_seq);
    match action {
        Action::Delete => {
            fs::remove_file(&path).map_err(|e| Error::io("delete retention segment", &path, e))?;
            *removed = true;
        }
        Action::MoveTo(dest) => move_segment(&path, dest, file, removed)?,
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
    let path = dir.join(journal::CHECKPOINT_FILE);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io("open checkpoint", &path, e)),
    };
    let slot = checkpoint(dir)?;
    if slot.is_some() && slot != *synced {
        crate::fault::inject(crate::fault::Point::RetentionMarkerSync)
            .and_then(|()| mmap::full_barrier(&file))
            .map_err(|e| Error::io("sync checkpoint marker", &path, e))?;
        *synced = slot;
    }
    Ok(slot)
}

/// Validate the position to the same extent as writer recovery, without scanning the mutable active tail.
pub(crate) fn checkpoint(dir: &Path) -> Result<Option<CheckpointSlot>> {
    let Some(slot) = inspect::read_checkpoint(dir)? else {
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
    Ok(Some(slot))
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
            return Ok(at >= first_slot + format::RECORD_HEADER_LEN as u64
                && commit == format::SEAL_MARKER
                && crc == format::seal_crc(successor));
        }
        end = start;
        chunk = (chunk * 2).min(TAIL_READ_MAX);
    }
    Ok(false)
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

fn move_segment(path: &Path, dest: &Path, source: &File, removed: &mut bool) -> Result<()> {
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
            sync_destination(dest)?;
        }
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            let mut copy = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map_err(|e| Error::io("create archive copy", &target, e))?;
            // pread leaves the source cursor untouched (the same File is also used for barriers).
            let mut offset = 0;
            let mut buffer = [0; 64 * 1024];
            use std::io::Write;
            loop {
                let count = source
                    .read_at(&mut buffer, offset)
                    .map_err(|e| Error::io("read archive source", path, e))?;
                if count == 0 {
                    break;
                }
                copy.write_all(&buffer[..count])
                    .map_err(|e| Error::io("write archive copy", &target, e))?;
                offset += count as u64;
            }
            crate::fault::inject(crate::fault::Point::RetentionCopySync)
                .map_err(|e| Error::io("sync archive copy", &target, e))?;
            mmap::sync_all(&copy).map_err(|e| Error::io("sync archive copy", &target, e))?;
            sync_destination(dest)?;
            mmap::full_barrier(&copy)
                .map_err(|e| Error::io("full sync archive copy", &target, e))?;
            fs::remove_file(path).map_err(|e| Error::io("remove archived segment", path, e))?;
            *removed = true;
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
