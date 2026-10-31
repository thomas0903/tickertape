//! Readers: replay and live tailing.

use std::path::Path;
use std::time::Duration;

use crate::config::ReaderConfig;
use crate::error::Result;
use crate::record::{OwnedRecord, Record};

/// An independent cursor over a journal, in this or another process.
///
/// Readers map segments read-only, never take locks, never write, and never slow the writer down. Any number can
/// run at once. Each record is validated (length, CRC32C, sequence number) before it is returned.
///
/// There are two ways to open one (ADR 0006):
/// - [`Reader::open`] (safe): each record is copied out of the mapping, with atomic loads, into a buffer the reader
///   reuses, and validated there. Memory-safe whatever other processes do to the files.
/// - [`Reader::open_mapped`] (`unsafe`, zero-copy): payloads point straight into the mapping.
///
/// `Reader` doesn't implement [`Iterator`], because returned records borrow from the reader. Loop with `while let`:
///
/// ```no_run
/// # use tickertape::{Reader, ReaderConfig};
/// # fn main() -> tickertape::Result<()> {
/// let mut reader = Reader::open("/tmp/journal", ReaderConfig::default())?;
/// while let Some(record) = reader.try_next()? {
///     println!("{} {} bytes", record.seq, record.payload.len());
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Reader {
    _private: (),
}

impl Reader {
    /// Opens the journal in `dir`, positioned at the first record of the oldest segment.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`](crate::Error::NotFound) if `dir` has no segments (for example, the writer hasn't started),
    /// [`Error::Corrupt`](crate::Error::Corrupt), [`Error::UnsupportedVersion`](crate::Error::UnsupportedVersion) if
    /// the oldest segment's header is invalid, [`Error::Io`](crate::Error::Io).
    pub fn open(dir: impl AsRef<Path>, config: ReaderConfig) -> Result<Reader> {
        let _ = (dir.as_ref(), config);
        todo!("M2")
    }

    /// Like [`Reader::open`], but returned records borrow their payload directly from the memory-mapped segment,
    /// without copying.
    ///
    /// # Safety
    ///
    /// While the returned reader exists, no process other than a tickertape [`Writer`](crate::Writer) may modify
    /// the journal's segment files (writing into them, truncating them, or replacing their contents through other
    /// tools). A tickertape writer never modifies bytes after committing them, including during crash recovery,
    /// which repairs by replacing files (`docs/format.md` → Recovery). Truncation by another process makes the
    /// process receive `SIGBUS` even with [`Reader::open`].
    ///
    /// # Errors
    ///
    /// As for [`Reader::open`].
    pub unsafe fn open_mapped(dir: impl AsRef<Path>, config: ReaderConfig) -> Result<Reader> {
        let _ = (dir.as_ref(), config);
        todo!("M2")
    }

    /// Positions the reader so that the next record returned is `seq`.
    ///
    /// If `seq` hasn't been written yet, the reader waits for it: [`Reader::try_next`] returns `None` until the
    /// writer gets there, and records before `seq` are skipped.
    ///
    /// # Errors
    ///
    /// [`Error::SeqUnavailable`](crate::Error::SeqUnavailable) if `seq` is older than the oldest segment;
    /// [`Error::Corrupt`](crate::Error::Corrupt) or [`Error::Io`](crate::Error::Io) while locating it.
    pub fn seek(&mut self, seq: u64) -> Result<()> {
        let _ = seq;
        todo!("M2")
    }

    /// Returns the next record if one is committed, without waiting.
    ///
    /// `Ok(None)` means there is no new record yet. Follows seals into the next segment transparently.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`](crate::Error::Corrupt) if the bytes at the cursor fail validation. The cursor doesn't move,
    /// so calling again retries the same position (which may succeed after a writer recovery).
    /// [`Error::Discarded`](crate::Error::Discarded) if writer recovery removed the segment at the cursor.
    /// [`Error::Io`](crate::Error::Io) if the next segment can't be opened.
    pub fn try_next(&mut self) -> Result<Option<Record<'_>>> {
        todo!("M2")
    }

    /// Returns the next record, polling with the configured [`Backoff`](crate::Backoff) until one is committed or
    /// `timeout` passes (`None`: wait forever). `Ok(None)` means the timeout passed.
    ///
    /// # Errors
    ///
    /// As for [`Reader::try_next`].
    pub fn next_timeout(&mut self, timeout: Option<Duration>) -> Result<Option<Record<'_>>> {
        let _ = timeout;
        todo!("M2")
    }

    /// Like [`Reader::try_next`], but copies the record out of the mapping.
    ///
    /// # Errors
    ///
    /// As for [`Reader::try_next`].
    pub fn next_owned(&mut self) -> Result<Option<OwnedRecord>> {
        Ok(self.try_next()?.map(|r| r.to_owned()))
    }

    /// The sequence number of the next record this reader will return.
    pub fn position(&self) -> u64 {
        todo!("M2")
    }
}
