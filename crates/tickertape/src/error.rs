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
    ///
    /// A reader also reports a read buffer it cannot allocate this way, with kind
    /// [`OutOfMemory`](std::io::ErrorKind::OutOfMemory): a frame too large for the address space of a 32-bit target.
    #[error("{op} failed for {}: {source}", path.display())]
    #[non_exhaustive]
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
    ///
    /// A writer releases the lock when it is dropped in the process that opened it (as does an open that fails after
    /// taking it). If that process exits or dies instead, the lock is released when the last copy of its descriptor
    /// closes, and a child the process forked (from any thread) keeps a copy until it calls `exec` or exits: briefly
    /// for a child that is about to run a program, for its whole life for one that never execs (a pre-fork worker;
    /// Python `multiprocessing` with the fork start method), and the same for a grandchild. A writer opened
    /// meanwhile sees this error (see `docs/crash-contract.md` → C3 and Known risks and limits).
    #[error("journal {} is locked by another writer (pid {pid:?})", dir.display())]
    #[non_exhaustive]
    Locked {
        /// The journal directory.
        dir: PathBuf,
        /// The PID recorded in `writer.lock`, if it could be read. Informational only.
        pid: Option<u32>,
    },

    /// Bytes in a segment failed validation.
    #[error("corrupt journal data in {} at offset {offset}: {reason}", path.display())]
    #[non_exhaustive]
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
    #[non_exhaustive]
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
    #[non_exhaustive]
    NotFound {
        /// The directory that was searched.
        dir: PathBuf,
    },

    /// A requested sequence number is older than the oldest segment in the journal.
    #[error("sequence number {requested} is before the first available record {first}")]
    #[non_exhaustive]
    SeqUnavailable {
        /// The requested sequence number.
        requested: u64,
        /// The oldest sequence number present.
        first: u64,
    },

    /// A payload does not fit in one segment.
    #[error("record of {len} payload bytes exceeds the maximum of {max} for this segment size")]
    #[non_exhaustive]
    RecordTooLarge {
        /// Requested payload length.
        len: usize,
        /// Maximum payload length for the configured segment size.
        max: usize,
    },

    /// The writer is poisoned after a failed checkpoint, or after the zeroing of an abandoned write could not be
    /// made durable ([`Writer::abandoned_write_error`](crate::Writer::abandoned_write_error)); durability of
    /// earlier records can't be guaranteed.
    ///
    /// Appends still work after a failed checkpoint. See `docs/crash-contract.md` → Errors are sticky.
    #[error(
        "an earlier checkpoint failed, or an abandoned write could not be zeroed durably; durability can no longer be \
         guaranteed for this writer"
    )]
    DurabilityLost,

    /// The next sequence number would be `u64::MAX`.
    #[error("sequence numbers exhausted")]
    SequenceExhausted,

    /// Writer recovery replaced or removed the segment, or retention removed it, at the reader's position,
    /// so records from this sequence number on may belong to a discarded history. Call
    /// [`Reader::seek`](crate::Reader::seek) to continue on the current chain.
    #[error("the segment at sequence {seq} was replaced or removed by recovery or retention")]
    #[non_exhaustive]
    Discarded {
        /// The reader's position when it noticed.
        seq: u64,
    },

    /// A configuration value is out of range.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    /// A reader beacon with this name is held by a running reader (or a process that inherited its descriptor), so
    /// [`ReaderBeacon::create`](crate::status::ReaderBeacon::create) won't take the name.
    #[error("reader beacon {} is held by a running reader", path.display())]
    #[non_exhaustive]
    BeaconInUse {
        /// The beacon file.
        path: PathBuf,
    },
}

impl Error {
    /// True if this error was caused by the filesystem being full (`ENOSPC`, `EDQUOT`).
    ///
    /// Appends that fail this way leave the journal consistent and can be retried once space is available.
    pub fn is_storage_full(&self) -> bool {
        matches!(self, Error::Io { source, .. } if crate::mmap::is_storage_full(source))
    }

    /// True for an I/O error saying a file or directory doesn't exist (`ENOENT`).
    pub(crate) fn is_not_found(&self) -> bool {
        matches!(self, Error::Io { source, .. } if source.kind() == io::ErrorKind::NotFound)
    }

    /// An [`Error::Io`]. Hidden: not covered by the compatibility promise; for the tests of the C ABI and the
    /// bindings, which cannot build the `#[non_exhaustive]` variant.
    #[doc(hidden)]
    pub fn io(op: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Error {
        Error::Io {
            op,
            path: path.into(),
            source,
        }
    }

    /// An [`Error::Corrupt`]. Hidden: not covered by the compatibility promise; for the tests of the C ABI and the
    /// bindings, which cannot build the `#[non_exhaustive]` variant.
    #[doc(hidden)]
    pub fn corrupt(path: impl Into<PathBuf>, offset: u64, reason: CorruptReason) -> Error {
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
    #[non_exhaustive]
    SizeMismatch {
        /// `segment_size` from the header.
        expected: u64,
        /// The file's actual size.
        actual: u64,
    },
    /// The file name and the header's `base_seq` disagree.
    #[error("file name says base sequence {from_name}, header says {from_header}")]
    #[non_exhaustive]
    BaseSeqMismatch {
        /// From the file name.
        from_name: u64,
        /// From the header.
        from_header: u64,
    },
    /// A slot is misaligned, before the record area, or outside the segment.
    #[error("invalid slot offset {0}")]
    BadOffset(u64),
    /// A record has no representable next sequence number.
    #[error("record sequence numbers exhausted")]
    SequenceExhausted,
    /// A record's length is out of range.
    #[error("record length {0} out of range")]
    BadLength(u32),
    /// A record's CRC32C doesn't match.
    #[error("record checksum mismatch")]
    BadChecksum,
    /// A record has an unexpected sequence number.
    #[error("expected sequence {expected}, found {found}")]
    #[non_exhaustive]
    BadSequence {
        /// The sequence number the position implies.
        expected: u64,
        /// The sequence number stored in the record.
        found: u64,
    },
    /// A reserved field is non-zero.
    #[error("reserved flags are non-zero")]
    BadFlags,
    /// A seal is invalid: its checksum doesn't match the expected next sequence number, or it lies where a seal
    /// can't. Writer recovery also reports it at the place of a missing seal in front of a successor segment that
    /// opens, when non-zero bytes follow beyond the seal's eight bytes (`docs/crash-contract.md`, C11 (1)).
    #[error("invalid seal")]
    BadSeal,
    /// The chain of segments is broken: a sealed segment's successor is missing or doesn't start where the seal
    /// says, a segment is disconnected from the chain, or the chain ends at an empty commit word in a segment that is
    /// not the newest. The number is the base the next segment in the chain should have, or, for a segment writer
    /// recovery found disconnected (reported at offset 0), that segment's own base. Writer recovery reports the last
    /// case at an empty commit word with non-zero bytes after it in front of a segment for the expected sequence number
    /// that exists but does not open, when its search finds no evidence of committed history after the word
    /// (`docs/crash-contract.md`, C11 (3)); `verify` reports it wherever the records of a segment other than the
    /// newest end at an empty commit word with no committed history found after it.
    #[error("next segment {0} missing or inconsistent")]
    BrokenChain(u64),
    /// The checkpoint marker points to a segment that doesn't exist, or to an impossible offset.
    #[error("checkpoint marker points to missing segment {0} or an invalid offset")]
    DanglingCheckpoint(u64),
    /// A commit word reads 0 (no record), and recovery or `verify` found evidence of committed history after it
    /// (`docs/crash-contract.md`, C11 (2); ADR 0035). The number is the sequence number the evidence carries: a later
    /// valid record's, a final seal's next sequence number, or (writer recovery only; `verify` does not weigh other
    /// segments) the base of a later segment. Usually storage damage, power loss or outside modification; a torn
    /// payload left by a writer process crash can also cause it (`docs/crash-contract.md`, C1).
    #[error("commit word is 0 but committed history follows it (sequence {0})")]
    EmptyCommitWord(u64),
    /// A commit word reads 0, and the search for committed history after it stopped at its work bound before it could
    /// decide (`docs/crash-contract.md`, C11 (2); ADR 0035 → Work bound). Inconclusive, unlike
    /// [`EmptyCommitWord`](Self::EmptyCommitWord): no record was proven valid, but recovery still treats the tail as
    /// committed history (preserved, or refused with `RecoveryPolicy::Fail`). Storage damage can cause it, and so
    /// can a torn payload left by a writer process crash (`docs/crash-contract.md`, C1). The number is the sequence
    /// number in the header of the candidate record where the search stopped, or the expected sequence number plus
    /// one when it stopped at a final seal.
    #[error(
        "commit word is 0 and the bytes after it were too costly to check completely (sequence {0}); treated as committed history"
    )]
    UncheckedTail(u64),
}
