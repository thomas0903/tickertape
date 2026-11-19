//! Read-only inspection and verification, used by the CLI (`tickertape inspect`, `tickertape verify`).
//!
//! Nothing here takes the writer lock or modifies files, so it is safe to run while a writer is active. Against a
//! live journal, results describe a moment in time and the newest segment may grow during the scan.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{CorruptReason, Error, Result};
use crate::format::{self, ScanEnd};
use crate::journal;

/// One segment file.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SegmentInfo {
    /// The file.
    pub path: PathBuf,
    /// Sequence number of its first record.
    pub base_seq: u64,
    /// Its size in bytes.
    pub segment_size: u64,
    /// Format version from its header.
    pub format_version: u16,
    /// Creation time from its header, nanoseconds since the Unix epoch.
    pub created_unix_ns: u64,
}

/// Lists the segments in `dir`, oldest first, with their decoded headers.
///
/// A segment that retention (or writer recovery) removes between the directory listing and the opening of its file is
/// not an error: the directory is listed again, so the result leaves it out. So is a listing that comes back empty on
/// a live journal (btrfs omits entries created after a listing starts). Both are retried at most 8 times: a listing
/// still empty on the eighth attempt gives [`Error::NotFound`], and a listed segment that vanished on the eighth an
/// I/O error. A segment removed after it was opened can still be in the result.
///
/// # Errors
///
/// [`Error::NotFound`] if there are none; [`Error::Corrupt`] or
/// [`Error::UnsupportedVersion`] for an invalid header;
/// [`Error::Io`].
pub fn list_segments(dir: impl AsRef<Path>) -> Result<Vec<SegmentInfo>> {
    let dir = dir.as_ref();
    let mut attempt = 1;
    loop {
        if let Some(segments) = list_once(dir, attempt == LISTING_ATTEMPTS)? {
            return Ok(segments);
        }
        attempt += 1;
    }
}

/// Listings [`list_segments`] and [`verify`] make when segments disappear while they run. Retention removes the
/// oldest segments first, so the next listing already reflects a removal; more attempts are needed only if files
/// keep disappearing, and on the last one a missing file is reported as an I/O error.
const LISTING_ATTEMPTS: u32 = 8;
/// [`verify`] scans a segment that ends in a state a live writer passes through again after this pause, then after
/// pauses twice as long as the one before, up to [`TRANSIENT_LAST_PAUSE`].
const TRANSIENT_FIRST_PAUSE: Duration = Duration::from_millis(10);
/// The longest of those pauses (10 + 20 + ... + 320 ms: 630 ms in all).
const TRANSIENT_LAST_PAUSE: Duration = Duration::from_millis(320);

/// Opens a listed segment. `Ok(None)` if it no longer exists, unless this is the `last` attempt.
fn open_listed(path: &Path, last: bool) -> Result<Option<File>> {
    crate::fault::hook(crate::fault::Point::InspectOpen);
    match File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !last => Ok(None),
        Err(e) => Err(Error::io("open segment", path, e)),
    }
}

/// Lists the segment files. `Ok(None)` if there are none, unless this is the `last` attempt ([`Error::NotFound`]):
/// on a live journal a directory read can come back empty, because btrfs returns only entries that existed when the
/// directory was opened and still exist when it is read.
fn list_nonempty(dir: &Path, last: bool) -> Result<Option<Vec<(u64, PathBuf)>>> {
    let segments = journal::list_segments(dir)?;
    match (segments.is_empty(), last) {
        (false, _) => Ok(Some(segments)),
        (true, false) => Ok(None),
        (true, true) => Err(Error::NotFound {
            dir: dir.to_path_buf(),
        }),
    }
}

/// One attempt at [`list_segments`]: `Ok(None)` if a listed segment disappeared.
fn list_once(dir: &Path, last: bool) -> Result<Option<Vec<SegmentInfo>>> {
    let Some(segments) = list_nonempty(dir, last)? else {
        return Ok(None);
    };
    let mut out = Vec::with_capacity(segments.len());
    for (base, path) in segments {
        let Some(file) = open_listed(&path, last)? else {
            return Ok(None);
        };
        let header = journal::read_header(&file, &path, base)?;
        out.push(SegmentInfo {
            path,
            base_seq: base,
            segment_size: header.segment_size,
            format_version: format::FORMAT_VERSION,
            created_unix_ns: header.created_unix_ns,
        });
    }
    Ok(Some(out))
}

/// Reads the current checkpoint marker without taking the writer lock or modifying the journal.
///
/// Returns `Ok(None)` when `checkpoint` is missing or neither slot is valid. A truncated marker may still
/// contain a valid slot. This is an observation of the marker, not validation of the position it describes. The
/// writer updates the marker in place and neither retention nor recovery removes it, so reading it can't race with
/// them (durable-only readers read it the same way).
///
/// # Errors
///
/// [`Error::Io`] if the marker cannot be opened or read for a reason other than a missing file.
pub fn read_checkpoint(dir: impl AsRef<Path>) -> Result<Option<format::CheckpointSlot>> {
    use std::io::Read;

    let path = dir.as_ref().join(journal::CHECKPOINT_FILE);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io("open checkpoint", &path, e)),
    };
    let mut bytes = Vec::new();
    file.take(format::CHECKPOINT_FILE_LEN as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::io("read checkpoint", &path, e))?;
    Ok(format::decode_checkpoint(&bytes))
}

/// How a segment ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SegmentEnd {
    /// Sealed: its successor holds the next record.
    Sealed,
    /// Not sealed: the newest segment, which the writer appends to.
    Open,
    /// Ends in a problem (listed in [`VerifyReport::problems`]).
    Damaged,
}

/// What [`verify`] found in one segment.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SegmentReport {
    /// The segment's header. `None` if the header is invalid.
    pub info: Option<SegmentInfo>,
    /// The file.
    pub path: PathBuf,
    /// Number of valid records.
    pub records: u64,
    /// Sequence number after the last valid record (equals the base if there are none).
    pub end_seq: u64,
    /// Bytes used by records (and the seal, if any), including the header.
    pub used_bytes: u64,
    /// How the segment ends.
    pub end: SegmentEnd,
}

/// The result of [`verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VerifyReport {
    /// Segments checked, oldest first.
    pub segments: Vec<SegmentReport>,
    /// Number of valid records found.
    pub records: u64,
    /// Sequence number of the first record, if any.
    pub first_seq: Option<u64>,
    /// Sequence number of the last valid record, if any.
    pub last_seq: Option<u64>,
    /// Every problem found. Empty means the journal is intact.
    pub problems: Vec<Problem>,
}

/// A problem found by [`verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Problem {
    /// The segment.
    pub path: PathBuf,
    /// Byte offset.
    pub offset: u64,
    /// What is wrong.
    pub reason: CorruptReason,
}

/// Scans every segment in `dir`, checking every header, record, seal and the chain between segments.
///
/// Uncommitted bytes after the end of the newest segment (left by a crashed writer, cleaned up by the next one) are
/// not a problem. Anything that a reader would reject is, and so are states that only a crash leaves behind and the
/// next writer repairs (a sealed newest segment without a successor, or an unsealed segment that has one).
///
/// Against a live journal, the report describes the segments listed when the scan started, as the scan found them.
/// A writer appending or rolling during the scan is not a problem: a segment that ends in a state a writer passes
/// through (a record being written at the end of the newest segment, or an unsealed segment whose successor already
/// exists, because a rolling writer links the successor just before it seals) is scanned again, after pauses of
/// 10 ms doubling up to 320 ms (630 ms in all), until it no longer is. So with a writer running, a problem in the
/// report is real unless the writer stayed in such a state for that long (descheduled or stopped between two
/// steps); run `verify` again to tell. A problem that is really there is always reported, only later. With no writer
/// running every reported problem is real, and a journal a crash left in such a state takes those 630 ms longer to
/// verify.
/// Retention is not a problem either: a segment it removes before the scan reaches it is not an error, and since
/// retention removes the oldest segments first, the report then starts at the oldest segment the scan could still
/// open, leaving out the removed history and what was found in it.
/// But a segment already open when retention removed it can stay in the report, with its records: the newest listed
/// segment always does, since nothing after it is opened. No problem is reported for its missing successor. As for
/// [`list_segments`], the listing is retried at most 8 times: still empty on the eighth attempt, the result is
/// [`Error::NotFound`].
///
/// # Errors
///
/// [`Error::NotFound`] or [`Error::Io`]. Corruption is reported in
/// [`VerifyReport::problems`], not as an error.
pub fn verify(dir: impl AsRef<Path>) -> Result<VerifyReport> {
    let dir = dir.as_ref();
    let mut attempt = 1;
    loop {
        if let Some(report) = verify_once(dir, attempt == LISTING_ATTEMPTS)? {
            return Ok(report);
        }
        attempt += 1;
    }
}

/// One attempt at [`verify`]: `Ok(None)` if the listing was empty, or a listed segment disappeared other than from
/// the oldest end, or the newest listed segment did.
fn verify_once(dir: &Path, last: bool) -> Result<Option<VerifyReport>> {
    let Some(segments) = list_nonempty(dir, last)? else {
        return Ok(None);
    };
    let mut report = VerifyReport {
        segments: Vec::new(),
        records: 0,
        first_seq: None,
        last_seq: None,
        problems: Vec::new(),
    };
    let mut expected_base = Some(segments[0].0);
    let count = segments.len();
    for (i, (base, path)) in segments.into_iter().enumerate() {
        let newest = i + 1 == count;
        let Some(file) = open_listed(&path, last)? else {
            // Removed after the listing. Removal from the oldest end (retention, archiving) also took everything
            // checked so far: start the report again from the next segment. Anything else: verify again.
            let oldest = journal::list_segments(dir)?.first().map(|(b, _)| *b);
            if newest || oldest.is_none_or(|oldest| base >= oldest) {
                return Ok(None);
            }
            report.segments.clear();
            report.problems.clear();
            expected_base = None;
            continue;
        };
        let mut seg = SegmentReport {
            info: None,
            path: path.clone(),
            records: 0,
            end_seq: base,
            used_bytes: 0,
            end: SegmentEnd::Damaged,
        };
        if let Some(expected) = expected_base.filter(|expected| *expected != base) {
            report.problems.push(Problem {
                path: path.clone(),
                offset: 0,
                reason: CorruptReason::BrokenChain(expected),
            });
        }
        let header = match journal::read_header(&file, &path, base) {
            Ok(h) => h,
            Err(Error::Corrupt { reason, offset, .. }) => {
                report.problems.push(Problem {
                    path: path.clone(),
                    offset,
                    reason,
                });
                report.segments.push(seg);
                continue;
            }
            Err(Error::UnsupportedVersion { .. }) => {
                report.problems.push(Problem {
                    path: path.clone(),
                    offset: 8,
                    reason: CorruptReason::BadSegmentHeader("unsupported format version"),
                });
                report.segments.push(seg);
                continue;
            }
            Err(e) => return Err(e),
        };
        seg.info = Some(SegmentInfo {
            path: path.clone(),
            base_seq: base,
            segment_size: header.segment_size,
            format_version: format::FORMAT_VERSION,
            created_unix_ns: header.created_unix_ns,
        });

        let mut scan = journal::scan_open(&file, &path, base)?;
        let transient = |end: ScanEnd| match end {
            // A live writer may have been mid-append while we copied the file.
            ScanEnd::Invalid(_) => newest,
            // A rolling writer publishes the next segment just before it seals this one.
            ScanEnd::Empty => !newest,
            _ => false,
        };
        // A writer descheduled in such a state stays in it: look again with growing pauses before reporting it.
        let mut pause = TRANSIENT_FIRST_PAUSE;
        while transient(scan.end) && pause <= TRANSIENT_LAST_PAUSE {
            std::thread::sleep(pause);
            scan = journal::scan_open(&file, &path, base)?;
            pause *= 2;
        }
        seg.records = scan.records;
        seg.end_seq = scan.next_seq;
        seg.used_bytes = scan.end_offset;
        match scan.end {
            ScanEnd::Seal => {
                seg.used_bytes += format::SEAL_LEN as u64;
                seg.end = SegmentEnd::Sealed;
                // A sealed newest segment is a problem only if its successor is missing while it is still in
                // place: if the writer rolled after the listing the successor exists now, and if retention has
                // since removed both (oldest first), this segment is gone too.
                if newest {
                    crate::fault::hook(crate::fault::Point::InspectSeal);
                    let successor = journal::segment_path(dir, scan.next_seq);
                    if journal::path_identity(&successor)?.is_none()
                        && journal::path_identity(&path)?
                            == Some(journal::file_identity(&file, &path)?)
                    {
                        report.problems.push(Problem {
                            path: path.clone(),
                            offset: scan.end_offset,
                            reason: CorruptReason::BrokenChain(scan.next_seq),
                        });
                    }
                }
            }
            ScanEnd::Empty if newest => seg.end = SegmentEnd::Open,
            ScanEnd::Empty => report.problems.push(Problem {
                path: path.clone(),
                offset: scan.end_offset,
                reason: CorruptReason::BrokenChain(scan.next_seq),
            }),
            ScanEnd::Invalid(reason) => report.problems.push(Problem {
                path: path.clone(),
                offset: scan.end_offset,
                reason,
            }),
        }
        expected_base = Some(scan.next_seq);
        report.segments.push(seg);
    }
    // Totals over the segments reported (records in a segment run from its base to `end_seq`).
    report.records = report.segments.iter().map(|s| s.records).sum();
    let mut with_records = report.segments.iter().filter(|s| s.records > 0);
    report.first_seq = with_records.next().map(|s| s.end_seq - s.records);
    report.last_seq = report
        .segments
        .iter()
        .rev()
        .find(|s| s.records > 0)
        .map(|s| s.end_seq - 1);
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_missing_invalid_valid_and_truncated() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_checkpoint(dir.path()).unwrap(), None);
        let path = dir.path().join("checkpoint");
        std::fs::write(&path, [0; 64]).unwrap();
        assert_eq!(read_checkpoint(dir.path()).unwrap(), None);
        let slot = format::CheckpointSlot {
            generation: 2,
            next_seq: 7,
            segment_base_seq: 0,
            offset: 512,
        };
        std::fs::write(&path, format::encode_checkpoint_slot(&slot)).unwrap();
        assert_eq!(read_checkpoint(dir.path()).unwrap(), Some(slot));
        std::fs::write(&path, [0; 63]).unwrap();
        assert_eq!(read_checkpoint(dir.path()).unwrap(), None);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(read_checkpoint(dir.path()), Err(Error::Io { .. })));
    }
}
