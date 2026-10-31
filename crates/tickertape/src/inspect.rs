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
/// # Errors
///
/// [`Error::NotFound`] if there are none; [`Error::Corrupt`] or
/// [`Error::UnsupportedVersion`] for an invalid header;
/// [`Error::Io`].
pub fn list_segments(dir: impl AsRef<Path>) -> Result<Vec<SegmentInfo>> {
    let dir = dir.as_ref();
    let segments = journal::list_segments(dir)?;
    if segments.is_empty() {
        return Err(Error::NotFound {
            dir: dir.to_path_buf(),
        });
    }
    segments
        .into_iter()
        .map(|(base, path)| {
            let file = File::open(&path).map_err(|e| Error::io("open segment", &path, e))?;
            let header = journal::read_header(&file, &path, base)?;
            Ok(SegmentInfo {
                path,
                base_seq: base,
                segment_size: header.segment_size,
                format_version: format::FORMAT_VERSION,
                created_unix_ns: header.created_unix_ns,
            })
        })
        .collect()
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
/// # Errors
///
/// [`Error::NotFound`] or [`Error::Io`]. Corruption is reported in
/// [`VerifyReport::problems`], not as an error.
pub fn verify(dir: impl AsRef<Path>) -> Result<VerifyReport> {
    let dir = dir.as_ref();
    let segments = journal::list_segments(dir)?;
    if segments.is_empty() {
        return Err(Error::NotFound {
            dir: dir.to_path_buf(),
        });
    }
    let mut report = VerifyReport {
        segments: Vec::new(),
        records: 0,
        first_seq: None,
        last_seq: None,
        problems: Vec::new(),
    };
    let mut expected_base = segments[0].0;
    let count = segments.len();
    for (i, (base, path)) in segments.into_iter().enumerate() {
        let newest = i + 1 == count;
        let mut seg = SegmentReport {
            info: None,
            path: path.clone(),
            records: 0,
            end_seq: base,
            used_bytes: 0,
            end: SegmentEnd::Damaged,
        };
        if base != expected_base {
            report.problems.push(Problem {
                path: path.clone(),
                offset: 0,
                reason: CorruptReason::BrokenChain(expected_base),
            });
        }
        let file = File::open(&path).map_err(|e| Error::io("open segment", &path, e))?;
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

        let mut scan = scan_file(&path, base)?;
        if newest && matches!(scan.end, ScanEnd::Invalid(_)) {
            // A live writer may have been mid-append while we copied the file: look again before reporting.
            std::thread::sleep(Duration::from_millis(50));
            scan = scan_file(&path, base)?;
        }
        seg.records = scan.records;
        seg.end_seq = scan.next_seq;
        seg.used_bytes = scan.end_offset;
        report.records += scan.records;
        if scan.records > 0 {
            report.first_seq.get_or_insert(base);
            report.last_seq = Some(scan.next_seq - 1);
        }
        match scan.end {
            ScanEnd::Seal => {
                seg.used_bytes += format::SEAL_LEN as u64;
                seg.end = SegmentEnd::Sealed;
                if newest {
                    report.problems.push(Problem {
                        path: path.clone(),
                        offset: scan.end_offset,
                        reason: CorruptReason::BrokenChain(scan.next_seq),
                    });
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
        expected_base = scan.next_seq;
        report.segments.push(seg);
    }
    Ok(report)
}

/// Scans a segment file from a private copy of its contents.
fn scan_file(path: &Path, base: u64) -> Result<format::ScanResult> {
    let image = std::fs::read(path).map_err(|e| Error::io("read segment", path, e))?;
    Ok(format::scan_segment(
        &image[..],
        format::SEGMENT_HEADER_LEN as u64,
        base,
    ))
}
