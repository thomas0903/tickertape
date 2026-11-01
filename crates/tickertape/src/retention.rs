//! Opt-in removal of a sealed, checkpoint-covered prefix of a journal (ADR 0016).
//!
//! [`plan`] is read-only. [`apply`] takes no writer lock, rechecks eligibility before each removal, and stops on
//! the first error. Limits are best effort: the checkpoint segment and everything after it are always retained.
//! A move never replaces a destination file, even an identical copy left by an interrupted move. Resolve that
//! conflict before retrying, or use a fresh destination. Destination directories must already exist.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::format::{self, CheckpointSlot, ScanEnd};
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
    /// Permanently delete the segment.
    Delete,
    /// Move it to an existing directory, preserving its basename. Existing destination entries are errors.
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

/// Plans eligible removals without changing files or taking the writer lock.
///
/// Only a contiguous sealed prefix strictly before a valid checkpoint segment qualifies. Age uses wall clock
/// time at planning; a nonmonotonic creation clock never allows skipping an ineligible oldest segment.
///
/// # Errors
///
/// [`Error::NotFound`] for an empty journal; [`Error::Corrupt`] or [`Error::UnsupportedVersion`] for an invalid
/// header, checkpoint position, or candidate chain; [`Error::Io`] for filesystem errors, including concurrent
/// changes while observing the directory. An absent/invalid checkpoint slot yields an empty plan.
pub fn plan(dir: impl AsRef<Path>, policy: &Policy) -> Result<Plan> {
    plan_at(dir.as_ref(), policy, crate::clock::realtime_ns())
}

fn plan_at(dir: &Path, policy: &Policy, now: u64) -> Result<Plan> {
    let directory = directory_identity(dir)?;
    let paths = journal::list_segments(dir)?;
    if paths.is_empty() {
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
    let headers = paths
        .iter()
        .map(|(base, path)| {
            let file = File::open(path).map_err(|e| Error::io("open segment", path, e))?;
            Ok((
                journal::read_header(&file, path, *base)?,
                journal::file_identity(&file, path)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut count = paths.len();
    let mut bytes: u128 = headers
        .iter()
        .map(|(h, _)| u128::from(h.segment_size))
        .sum();
    for (i, (base, path)) in paths.iter().enumerate() {
        if *base >= checkpoint.segment_base_seq || i + 1 == paths.len() {
            break;
        }
        let (header, identity) = headers[i];
        let sealed = headers[i + 1].0.created_unix_ns;
        let reasons = reasons(policy, count, bytes, now, sealed);
        if reasons.is_empty() {
            break;
        }
        validate_sealed(path, *base, paths[i + 1].0)?;
        plan.segments.push(PlannedSegment {
            path: path.clone(),
            base_seq: *base,
            size_bytes: header.segment_size,
            sealed_unix_ns: sealed,
            reasons,
            identity,
            successor: paths[i + 1].0,
        });
        count -= 1;
        bytes -= u128::from(header.segment_size);
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
/// Same-filesystem moves use an atomic no-replace rename. On `EXDEV`, a newly created destination copy is
/// synced, its directory synced, then the source removed and the journal directory synced. Partial copies stay
/// in the destination on error; they never justify removing the source. No existing destination is replaced.
///
/// # Errors
///
/// [`Error::InvalidConfig`] for a plan used on a different/replaced journal, or lost eligibility;
/// [`Error::Corrupt`] / [`Error::UnsupportedVersion`] for invalid candidate data or checkpoint positions;
/// [`Error::Io`] for I/O errors (including destination conflicts). Stops immediately, retaining earlier effects.
/// A missing source counts as already removed only when no older segment remains.
pub fn apply(dir: impl AsRef<Path>, plan: &Plan, action: &Action) -> Result<Report> {
    let mut report = Report::default();
    apply_into(dir.as_ref(), plan, action, &mut report)?;
    Ok(report)
}

pub(crate) fn apply_into(
    dir: &Path,
    plan: &Plan,
    action: &Action,
    report: &mut Report,
) -> Result<()> {
    if directory_identity(dir)? != plan.directory {
        return Err(Error::InvalidConfig(
            "retention plan belongs to a different journal directory".into(),
        ));
    }
    for segment in &plan.segments {
        let paths = journal::list_segments(dir)?;
        // A missing file is harmless only if the entire older prefix has also gone.
        if already_removed(&paths, segment.base_seq) {
            report.already_removed_segments = report.already_removed_segments.saturating_add(1);
            continue;
        }
        if paths.first().map(|(base, _)| *base) != Some(segment.base_seq) {
            return Err(Error::corrupt(
                journal::segment_path(dir, segment.base_seq),
                0,
                CorruptReason::BrokenChain(segment.base_seq),
            ));
        }
        let Some(marker) = checkpoint(dir)? else {
            return Err(Error::InvalidConfig(
                "retention requires a valid checkpoint".into(),
            ));
        };
        if segment.base_seq >= marker.segment_base_seq {
            return Err(Error::InvalidConfig(
                "planned segment is no longer older than the checkpoint".into(),
            ));
        }
        let path = journal::segment_path(dir, segment.base_seq);
        let mut removed = false;
        let result = (|| {
            let file =
                File::open(&path).map_err(|e| Error::io("open retention segment", &path, e))?;
            if journal::file_identity(&file, &path)? != segment.identity {
                return Err(Error::InvalidConfig(
                    "planned segment has been replaced".into(),
                ));
            }
            journal::read_header(&file, &path, segment.base_seq)?;
            // Check the expected successor before any destructive operation; a missing middle is never tolerated.
            let successor = journal::segment_path(dir, segment.successor);
            if paths.get(1).map(|(base, _)| *base) != Some(segment.successor) {
                return Err(Error::corrupt(
                    &path,
                    0,
                    CorruptReason::BrokenChain(segment.successor),
                ));
            }
            let next = File::open(&successor)
                .map_err(|e| Error::io("open retention successor", &successor, e))?;
            journal::read_header(&next, &successor, segment.successor)?;
            validate_sealed(&path, segment.base_seq, segment.successor)?;
            match action {
                Action::Delete => {
                    fs::remove_file(&path)
                        .map_err(|e| Error::io("delete retention segment", &path, e))?;
                    removed = true;
                }
                Action::MoveTo(dest) => move_segment(&path, dest, &file, &mut removed)?,
            }
            journal::sync_dir(dir)?;
            mmap::full_barrier(&file)
                .map_err(|e| Error::io("full sync retention segment", &path, e))?;
            Ok(())
        })();
        // Count the unlink/rename even if a subsequent sync fails.
        if removed {
            report.removed_segments = report.removed_segments.saturating_add(1);
            report.removed_bytes = report.removed_bytes.saturating_add(segment.size_bytes);
        }
        if let Err(error) = result {
            if !removed
                && matches!(&error, Error::Io { source, .. } if source.kind() == io::ErrorKind::NotFound)
                && already_removed(&journal::list_segments(dir)?, segment.base_seq)
            {
                report.already_removed_segments = report.already_removed_segments.saturating_add(1);
                continue;
            }
            return Err(error);
        }
    }
    Ok(())
}

fn already_removed(paths: &[(u64, PathBuf)], base: u64) -> bool {
    paths.first().is_some_and(|(oldest, _)| base < *oldest)
}

fn directory_identity(dir: &Path) -> Result<journal::Identity> {
    let meta = fs::metadata(dir).map_err(|e| Error::io("stat journal directory", dir, e))?;
    Ok((meta.dev(), meta.ino()))
}

/// Validate the position to the same extent as writer recovery, without scanning the mutable active tail.
fn checkpoint(dir: &Path) -> Result<Option<CheckpointSlot>> {
    let Some(slot) = inspect::read_checkpoint(dir)? else {
        return Ok(None);
    };
    let path = journal::segment_path(dir, slot.segment_base_seq);
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

fn validate_sealed(path: &Path, base: u64, successor: u64) -> Result<()> {
    let image = fs::read(path).map_err(|e| Error::io("read retention segment", path, e))?;
    let scan = format::scan_segment(&image[..], format::SEGMENT_HEADER_LEN as u64, base);
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
}
