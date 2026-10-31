//! Error type.

use std::io;
use std::path::PathBuf;

/// Result type of all fallible tickertape operations.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong.
///
/// No tickertape function panics on bad file contents or OS errors; they return one of these.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An operating-system call failed.
    #[error("{op} failed for {}: {source}", path.display())]
    Io {
        /// What tickertape was doing, e.g. `"preallocate segment"`.
        op: &'static str,
        /// The file or directory involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// Another writer holds the journal's lock.
    #[error("journal {} is locked by another writer (pid {pid:?})", dir.display())]
    Locked {
        /// The journal directory.
        dir: PathBuf,
        /// The PID recorded in `writer.lock`, if it could be read. Informational only.
        pid: Option<u32>,
    },

    /// Bytes in a segment failed validation.
    #[error("corrupt journal data in {} at offset {offset}: {reason}", path.display())]
    Corrupt {
        /// The segment file.
        path: PathBuf,
        /// Byte offset of the invalid header, record or seal.
        offset: u64,
        /// What was wrong.
        reason: CorruptReason,
    },

    /// A segment was written by an incompatible format version.
    #[error("{} has format version {found}; this build supports version {supported}", path.display())]
    UnsupportedVersion {
        /// The segment file.
        path: PathBuf,
        /// The version in the file.
        found: u16,
        /// The version this build reads and writes.
        supported: u16,
    },

    /// The directory contains no segments.
    #[error("no journal segments found in {}", dir.display())]
    NotFound {
        /// The directory that was searched.
        dir: PathBuf,
    },

    /// A requested sequence number is older than the oldest segment in the journal.
    #[error("sequence number {requested} is before the first available record {first}")]
    SeqUnavailable {
        /// The requested sequence number.
        requested: u64,
        /// The oldest sequence number present.
        first: u64,
    },

    /// A payload does not fit in one segment.
    #[error("record of {len} payload bytes exceeds the maximum of {max} for this segment size")]
    RecordTooLarge {
        /// Requested payload length.
        len: usize,
        /// Maximum payload length for the configured segment size.
        max: usize,
    },

    /// The writer is poisoned after a failed checkpoint; durability of earlier records can't be guaranteed.
    ///
    /// Appends still work. See `docs/crash-contract.md` → Errors are sticky.
    #[error("an earlier checkpoint failed; durability can no longer be guaranteed for this writer")]
    DurabilityLost,

    /// The next sequence number would be `u64::MAX`.
    #[error("sequence numbers exhausted")]
    SequenceExhausted,

    /// Writer recovery replaced or removed the segment at the reader's position (after a power loss or corruption),
    /// so records from this sequence number on may belong to a discarded history. Call
    /// [`Reader::seek`](crate::Reader::seek) to continue on the current chain.
    #[error("the segment at sequence {seq} was replaced or removed by writer recovery")]
    Discarded {
        /// The reader's position when it noticed.
        seq: u64,
    },

    /// A configuration value is out of range.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
}

impl Error {
    /// True if this error was caused by the filesystem being full (`ENOSPC`, `EDQUOT`).
    ///
    /// Appends that fail this way leave the journal consistent and can be retried once space is available.
    pub fn is_storage_full(&self) -> bool {
        matches!(self, Error::Io { source, .. } if crate::mmap::is_storage_full(source))
    }

    pub(crate) fn io(op: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Error {
        Error::Io {
            op,
            path: path.into(),
            source,
        }
    }

    pub(crate) fn corrupt(path: impl Into<PathBuf>, offset: u64, reason: CorruptReason) -> Error {
        Error::Corrupt {
            path: path.into(),
            offset,
            reason,
        }
    }
}

/// Why bytes failed validation. See `docs/format.md` for the rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CorruptReason {
    /// The segment header's magic bytes are wrong.
    #[error("bad segment magic")]
    BadMagic,
    /// The segment header failed its checksum or one of its fields is out of range.
    #[error("invalid segment header: {0}")]
    BadSegmentHeader(&'static str),
    /// The segment file is shorter or longer than its header says.
    #[error("segment file size {actual} differs from header size {expected}")]
    SizeMismatch {
        /// `segment_size` from the header.
        expected: u64,
        /// The file's actual size.
        actual: u64,
    },
    /// The file name and the header's `base_seq` disagree.
    #[error("file name says base sequence {from_name}, header says {from_header}")]
    BaseSeqMismatch {
        /// From the file name.
        from_name: u64,
        /// From the header.
        from_header: u64,
    },
    /// A record's length is out of range.
    #[error("record length {0} out of range")]
    BadLength(u32),
    /// A record's CRC32C doesn't match.
    #[error("record checksum mismatch")]
    BadChecksum,
    /// A record has an unexpected sequence number.
    #[error("expected sequence {expected}, found {found}")]
    BadSequence {
        /// The sequence number the position implies.
        expected: u64,
        /// The sequence number stored in the record.
        found: u64,
    },
    /// A reserved field is non-zero.
    #[error("reserved flags are non-zero")]
    BadFlags,
    /// A seal's checksum doesn't match the expected next sequence number.
    #[error("invalid seal")]
    BadSeal,
    /// A sealed segment's successor is missing while later segments exist, or doesn't start where the seal says.
    #[error("next segment {0} missing or inconsistent")]
    BrokenChain(u64),
    /// The checkpoint marker points to a segment that doesn't exist, or to an impossible offset.
    #[error("checkpoint marker points to missing segment {0} or an invalid offset")]
    DanglingCheckpoint(u64),
}
