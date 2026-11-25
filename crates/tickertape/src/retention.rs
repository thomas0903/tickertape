//! Opt-in removal of a sealed, checkpoint-covered prefix of a journal (ADR 0016).
//!
//! [`plan`] is read-only. [`apply`] takes no writer lock, rechecks eligibility before each removal, and stops on
//! the first error. Limits are best effort: the checkpoint segment and everything after it are always retained.
//! A move never replaces a destination file, even an identical copy left by an interrupted move. Resolve that
//! conflict before retrying, or use a fresh destination. Destination directories must already exist.
//!
//! What is read of a segment before it goes (ADR 0044, crash contract C14): a deletion, and [`plan`], check its header
//! and its successor's, and that its last non-zero bytes are a valid seal naming the successor, without reading its
//! records; a move reads the whole segment and validates every record and the seal first, as the copy it leaves must be
//! a valid segment.

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
/// Reads every segment's header, and of each candidate the end of the file back to its seal (the bytes after the
/// seal are zeros): a candidate is sealed when its last non-zero eight bytes are a valid seal naming the next segment.
/// Its records are not read, so a damaged record inside a sealed candidate does not stop the plan; a move
/// ([`Action::MoveTo`]) reads and validates the whole segment before moving it. When the end of a candidate is not
/// such a seal, the whole segment is scanned as before, and the error names what the scan found.
///
/// # Errors
///
/// [`Error::NotFound`] for an empty journal; [`Error::Corrupt`] or [`Error::UnsupportedVersion`] for an invalid
/// header, checkpoint position, or candidate seal or chain; [`Error::Io`] for filesystem errors, including concurrent
/// changes while observing the directory. An absent/invalid checkpoint slot yields an empty plan.
pub fn plan(dir: impl AsRef<Path>, policy: &Policy) -> Result<Plan> {
    plan_at(dir.as_ref(), policy, crate::clock::realtime_ns(), &|_| {
        false
    })
}

/// [`plan`] for the writer's retention run on its checkpoint thread: once at least one segment is planned, planning
/// stops as soon as `stop(planned)` returns true for the number of segments planned so far (ADR 0044).
pub(crate) fn plan_until(
    dir: &Path,
    policy: &Policy,
    stop: &dyn Fn(usize) -> bool,
) -> Result<Plan> {
    plan_at(dir, policy, crate::clock::realtime_ns(), stop)
}

fn plan_at(dir: &Path, policy: &Policy, now: u64, stop: &dyn Fn(usize) -> bool) -> Result<Plan> {
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
        if reasons.is_empty() || (!plan.segments.is_empty() && stop(plan.segments.len())) {
            break;
        }
        let file = File::open(path).map_err(|e| Error::io("open segment", path, e))?;
        check_seal(&file, path, header.segment_size, *base, paths[i + 1].0)?;
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
/// Before each removal: the segment is the oldest in the journal and still the file planned (same identity), its
/// header and its successor's are valid, the successor is the next segment in the journal, and the segment is
/// strictly older than the checkpoint segment. [`Action::Delete`] then checks the seal as [`plan`] does, without
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
    apply_into(dir.as_ref(), plan, action, &mut report, true, None)?;
    Ok(report)
}

/// `external` is true for a caller other than the writer itself (which runs retention only after its own marker is
/// durable): the marker that decides each removal is then made durable first ([`durable_checkpoint`]).
///
/// `stop` is given by the writer's retention run on its checkpoint thread: after the first planned segment, the run
/// ends before the next one once `stop(handled)` returns true for the number of planned segments handled so far (a
/// checkpoint is waiting and the run has removed what the checkpoint before it covered, or the writer is closing;
/// ADR 0044). The rest of the plan is left for the next run, which plans again.
pub(crate) fn apply_into(
    dir: &Path,
    plan: &Plan,
    action: &Action,
    report: &mut Report,
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
        let marker = if external {
            durable_checkpoint(dir, &mut synced)?
        } else {
            checkpoint(dir)?
        };
        let Some(marker) = marker else {
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
            let header = journal::read_header(&file, &path, segment.base_seq)?;
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
            match action {
                Action::Delete => check_seal(
                    &file,
                    &path,
                    header.segment_size,
                    segment.base_seq,
                    segment.successor,
                )?,
                Action::MoveTo(_) => {
                    check_records(&file, &path, segment.base_seq, segment.successor)?
                }
            }
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
            // Its seek index, best effort (ADR 0045): a leftover is harmless, and the writer's next open removes it.
            crate::seek_index::dir::remove_index(dir, segment.base_seq);
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
