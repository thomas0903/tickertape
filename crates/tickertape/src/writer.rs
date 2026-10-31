//! The single writer.

use std::path::{Path, PathBuf};

use crate::config::WriterConfig;
use crate::error::Result;
use crate::record::NewRecord;

/// The only process allowed to append to a journal.
///
/// Holds an exclusive `flock` on `writer.lock` for as long as it lives; the OS releases it when the process exits
/// or dies, so a crashed writer never leaves a stale lock. `Writer` is `Send` but not `Sync`: appends take `&mut self`.
///
/// Dropping a writer does **not** checkpoint. Everything appended stays visible and survives the process exiting,
/// but is durable only after [`Writer::checkpoint`]. See `docs/crash-contract.md`.
#[derive(Debug)]
pub struct Writer {
    _private: (),
}

impl Writer {
    /// Opens the journal in `dir` for writing, creating the directory and the first segment if needed.
    ///
    /// Takes the lock, then recovers the journal as described in `docs/format.md` → Recovery: validates the chain
    /// of records from the last checkpoint marker (or the oldest segment, if there is none) to the end, repairs or
    /// quarantines what is invalid, and syncs every repair before returning. What it did is in
    /// [`Writer::recovery_report`]. Open time is proportional to the data written since the last checkpoint.
    ///
    /// # Errors
    ///
    /// - [`Error::Locked`](crate::Error::Locked) if another writer has the journal open.
    /// - [`Error::InvalidConfig`](crate::Error::InvalidConfig) if `config.segment_size` breaks the format's limits.
    /// - [`Error::Corrupt`](crate::Error::Corrupt) if the journal is damaged in a way recovery doesn't repair, or
    ///   the tail is corrupt and the policy is [`RecoveryPolicy::Fail`](crate::RecoveryPolicy::Fail).
    /// - [`Error::UnsupportedVersion`](crate::Error::UnsupportedVersion), [`Error::Io`](crate::Error::Io).
    pub fn open(dir: impl AsRef<Path>, config: WriterConfig) -> Result<Writer> {
        let _ = (dir.as_ref(), config);
        todo!("M2")
    }

    /// Appends a record and returns its sequence number.
    ///
    /// When this returns `Ok`, the record is committed: visible to all readers and safe from a crash of this process.
    /// It is not durable until a later [`Writer::checkpoint`]. If the record doesn't fit in the current segment, this
    /// call rolls to a new segment first (slower: it creates and syncs a file).
    ///
    /// # Errors
    ///
    /// - [`Error::RecordTooLarge`](crate::Error::RecordTooLarge) if the payload can't fit in one segment, or doesn't
    ///   fit in the current segment and that segment is still empty (see `docs/format.md` → Rolling).
    /// - [`Error::SequenceExhausted`](crate::Error::SequenceExhausted) after `u64::MAX - 1`.
    /// - [`Error::Io`](crate::Error::Io) if a roll fails, e.g. the disk is full
    ///   ([`Error::is_storage_full`](crate::Error::is_storage_full)). The journal is unchanged and the call can be
    ///   retried.
    pub fn append(&mut self, record: &NewRecord<'_>) -> Result<u64> {
        let _ = record;
        todo!("M2")
    }

    /// Appends a record whose payload is written in place by `fill`, avoiding a copy.
    ///
    /// `fill` receives a zeroed buffer of exactly `payload_len` bytes inside the mapped segment. No reader can see
    /// it until `fill` returns and the record is committed. If `fill` panics, nothing is committed: the writer zeroes
    /// the buffer and syncs that zeroing (`docs/format.md` → Abandoned writes) before the panic continues, so no
    /// bytes from it can later be mistaken for a record. `record.payload` is ignored.
    ///
    /// # Errors
    ///
    /// As for [`Writer::append`].
    pub fn append_with<F>(
        &mut self,
        record: &NewRecord<'_>,
        payload_len: usize,
        fill: F,
    ) -> Result<u64>
    where
        F: FnOnce(&mut [u8]),
    {
        let _ = (record, payload_len, fill);
        todo!("M2")
    }

    /// Makes every record appended so far durable, using the platform's sync calls (see
    /// `docs/crash-contract.md` → What `checkpoint()` does). Blocks until they finish.
    ///
    /// # Errors
    ///
    /// [`Error::Io`](crate::Error::Io) if a sync call fails. The writer is then poisoned: every later call returns
    /// [`Error::DurabilityLost`](crate::Error::DurabilityLost), because a failed sync can drop data that a retry
    /// would not rewrite. Appends keep working.
    pub fn checkpoint(&mut self) -> Result<()> {
        todo!("M2")
    }

    /// The sequence number the next append will get. Equals the number of records in the journal.
    pub fn next_seq(&self) -> u64 {
        todo!("M2")
    }

    /// What recovery did when this writer opened the journal.
    pub fn recovery_report(&self) -> &RecoveryReport {
        todo!("M2")
    }

    /// The journal directory.
    pub fn dir(&self) -> &Path {
        todo!("M2")
    }
}

/// What [`Writer::open`] found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RecoveryReport {
    /// Sequence number of the next record after recovery.
    pub next_seq: u64,
    /// Where validation started: the checkpoint marker's `next_seq`, or the oldest segment's `base_seq`.
    pub scanned_from_seq: u64,
    /// Whether a valid checkpoint marker was found.
    pub had_checkpoint: bool,
    /// Every repair action, in the order performed. Empty for a cleanly closed journal.
    pub actions: Vec<RecoveryAction>,
}

/// One repair performed by [`Writer::open`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecoveryAction {
    /// The journal didn't exist and was created.
    Created,
    /// Uncommitted bytes after the last record (a record being written when the previous writer died) were zeroed.
    ZeroedUncommitted {
        /// The segment.
        segment: PathBuf,
        /// First zeroed offset.
        offset: u64,
        /// Number of bytes zeroed.
        len: u64,
    },
    /// A corrupt committed tail was copied to `quarantine`, and the segment was replaced by a copy with the tail
    /// zeroed (never edited in place).
    RepairedTail {
        /// The segment.
        segment: PathBuf,
        /// First affected offset.
        offset: u64,
        /// Number of bytes moved.
        len: u64,
        /// The quarantine file.
        quarantine_file: PathBuf,
        /// Why the record at `offset` was invalid.
        reason: crate::CorruptReason,
    },
    /// A segment with an invalid header, or one disconnected from the valid chain, was moved to `quarantine`.
    QuarantinedSegment {
        /// Where the segment was.
        segment: PathBuf,
        /// Where it is now.
        quarantine_file: PathBuf,
        /// Why it was removed from the chain.
        reason: crate::CorruptReason,
    },
    /// The previous segment was missing its seal (crash during a roll) and was sealed.
    Sealed {
        /// The segment that was sealed.
        segment: PathBuf,
    },
    /// The newest segment was sealed but its successor was missing; the successor was created.
    CompletedRoll {
        /// The new segment.
        segment: PathBuf,
    },
    /// A stale `*.tape.tmp` file from an interrupted roll was removed.
    RemovedTemp {
        /// The removed file.
        file: PathBuf,
    },
}
