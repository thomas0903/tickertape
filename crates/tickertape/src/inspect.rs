//! Read-only inspection and verification, used by the CLI (`tickertape inspect`, `tickertape verify`).
//!
//! Nothing here takes the writer lock or modifies files, so it is safe to run while a writer is active. Against a
//! live journal, results describe a moment in time and the newest segment may grow during the scan.

use std::path::{Path, PathBuf};

use crate::error::{CorruptReason, Result};

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
/// [`Error::NotFound`](crate::Error::NotFound) if there are none; [`Error::Corrupt`](crate::Error::Corrupt) or
/// [`Error::UnsupportedVersion`](crate::Error::UnsupportedVersion) for an invalid header;
/// [`Error::Io`](crate::Error::Io).
pub fn list_segments(dir: impl AsRef<Path>) -> Result<Vec<SegmentInfo>> {
    let _ = dir.as_ref();
    todo!("M5")
}

/// The result of [`verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VerifyReport {
    /// Segments checked, oldest first.
    pub segments: Vec<SegmentInfo>,
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
/// not a problem. Anything that a reader would reject is.
///
/// # Errors
///
/// [`Error::NotFound`](crate::Error::NotFound) or [`Error::Io`](crate::Error::Io). Corruption is reported in
/// [`VerifyReport::problems`], not as an error.
pub fn verify(dir: impl AsRef<Path>) -> Result<VerifyReport> {
    let _ = dir.as_ref();
    todo!("M5")
}
