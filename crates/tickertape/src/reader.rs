//! Readers: replay and live tailing.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{Ordering, fence};
use std::time::{Duration, Instant};

use crate::config::{Backoff, ReaderConfig};
use crate::error::{CorruptReason, Error, Result};
use crate::format::{self, RecordHeader};
use crate::journal::{self, Identity};
use crate::mmap::MapRo;
use crate::record::{OwnedRecord, Record};

/// How long a reader waits at an empty slot before checking that its segment wasn't replaced by recovery.
const IDENTITY_CHECK_INTERVAL: Duration = Duration::from_millis(100);
/// Bytes the `pread` reader fetches at a time (more if a record is larger).
const CHUNK: usize = 64 * 1024;
/// Re-reads before a validation failure is reported as corruption (the copy may predate the commit).
const VALIDATION_RETRIES: u32 = 3;

/// An independent cursor over a journal, in this or another process.
///
/// Readers never take locks, never write, and never slow the writer down. Any number can run at once. Each record
/// is validated (length, CRC32C, sequence number) before it is returned.
///
/// There are two ways to open one (ADR 0006):
/// - [`Reader::open`] (safe): reads the files with `pread` into a buffer the reader owns, and validates each record
///   there. Memory-safe whatever other processes do to the files, and never receives `SIGBUS`.
/// - [`Reader::open_mapped`] (`unsafe`, zero-copy): payloads point straight into a read-only mapping.
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
    dir: PathBuf,
    config: ReaderConfig,
    mapped: bool,
    seg: OpenSegment,
    /// Offset of the next slot in `seg`.
    offset: u64,
    /// Sequence number expected at `offset`.
    next_seq: u64,
    /// Records before this are skipped (set by `seek`).
    skip_until: u64,
    /// `pread` buffer: bytes of `seg` starting at file offset `buf_start`.
    buf: Vec<u8>,
    buf_start: u64,
    /// When the reader started waiting at an empty slot.
    waiting_since: Option<Instant>,
}

#[derive(Debug)]
struct OpenSegment {
    path: PathBuf,
    size: u64,
    file: File,
    identity: Identity,
    map: Option<MapRo>,
}

/// A record found by [`Reader::advance`]: its header and where its payload is.
#[derive(Debug, Clone, Copy)]
struct Found {
    header: RecordHeader,
    /// Offset of the payload in the segment file.
    payload_at: u64,
    payload_len: usize,
}

/// What is at the cursor.
#[derive(Debug, Clone, Copy)]
enum SlotOutcome {
    Empty,
    Record { header: RecordHeader, len: u32 },
    Seal,
    Invalid(CorruptReason),
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
        Reader::open_inner(dir.as_ref(), config, false)
    }

    /// Like [`Reader::open`], but returned records borrow their payload directly from the memory-mapped segment,
    /// without copying.
    ///
    /// # Safety
    ///
    /// While the returned reader exists, no process other than a tickertape [`Writer`](crate::Writer) may modify
    /// the journal's segment files (writing into them, truncating them, or replacing their contents through other
    /// tools). A tickertape writer never modifies bytes after committing them, including during crash recovery,
    /// which repairs by replacing files (`docs/format.md` → Recovery). If a mapped file is truncated, the process
    /// receives `SIGBUS`.
    ///
    /// # Errors
    ///
    /// As for [`Reader::open`].
    pub unsafe fn open_mapped(dir: impl AsRef<Path>, config: ReaderConfig) -> Result<Reader> {
        Reader::open_inner(dir.as_ref(), config, true)
    }

    fn open_inner(dir: &Path, config: ReaderConfig, mapped: bool) -> Result<Reader> {
        let segments = journal::list_segments(dir)?;
        let Some((base, _)) = segments.first() else {
            return Err(Error::NotFound {
                dir: dir.to_path_buf(),
            });
        };
        let seg = open_segment(dir, *base, mapped)?;
        Ok(Reader {
            dir: dir.to_path_buf(),
            config,
            mapped,
            seg,
            offset: format::SEGMENT_HEADER_LEN as u64,
            next_seq: *base,
            skip_until: *base,
            buf: Vec::new(),
            buf_start: 0,
            waiting_since: None,
        })
    }

    /// Positions the reader so that the next record returned is `seq`.
    ///
    /// If `seq` hasn't been written yet, the reader waits for it: [`Reader::try_next`] returns `None` until the
    /// writer gets there, and records before `seq` are skipped. Every record on the way is validated, so the reader
    /// only lands on positions along the journal's current chain.
    ///
    /// # Errors
    ///
    /// [`Error::SeqUnavailable`](crate::Error::SeqUnavailable) if `seq` is older than the oldest segment;
    /// [`Error::Corrupt`](crate::Error::Corrupt) or [`Error::Io`](crate::Error::Io) while locating it.
    pub fn seek(&mut self, seq: u64) -> Result<()> {
        let segments = journal::list_segments(&self.dir)?;
        let Some((first, _)) = segments.first() else {
            return Err(Error::NotFound {
                dir: self.dir.clone(),
            });
        };
        let Some((base, _)) = segments.iter().rev().find(|(b, _)| *b <= seq) else {
            return Err(Error::SeqUnavailable {
                requested: seq,
                first: *first,
            });
        };
        self.seg = open_segment(&self.dir, *base, self.mapped)?;
        self.offset = format::SEGMENT_HEADER_LEN as u64;
        self.next_seq = *base;
        self.skip_until = seq;
        self.buf.clear();
        self.waiting_since = None;
        Ok(())
    }

    /// Returns the next record if one is committed, without waiting.
    ///
    /// `Ok(None)` means there is no new record yet. Follows seals into the next segment transparently.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`](crate::Error::Corrupt) if the bytes at the cursor fail validation. The cursor doesn't move,
    /// so calling again retries the same position (which may succeed after a writer recovery).
    /// [`Error::Discarded`](crate::Error::Discarded) if writer recovery replaced or removed the segment at the cursor;
    /// call [`Reader::seek`] to continue on the current chain.
    /// [`Error::Io`](crate::Error::Io) if the next segment can't be opened.
    pub fn try_next(&mut self) -> Result<Option<Record<'_>>> {
        match self.advance()? {
            Some(found) => Ok(Some(self.materialize(found))),
            None => Ok(None),
        }
    }

    /// Returns the next record, polling with the configured [`Backoff`](crate::Backoff) until one is committed or
    /// `timeout` passes (`None`: wait forever). `Ok(None)` means the timeout passed.
    ///
    /// # Errors
    ///
    /// As for [`Reader::try_next`].
    pub fn next_timeout(&mut self, timeout: Option<Duration>) -> Result<Option<Record<'_>>> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut waiter = Waiter::new(self.config.backoff);
        loop {
            if let Some(found) = self.advance()? {
                return Ok(Some(self.materialize(found)));
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(None);
            }
            waiter.wait(deadline);
        }
    }

    /// Like [`Reader::try_next`], but copies the record out of the reader.
    ///
    /// # Errors
    ///
    /// As for [`Reader::try_next`].
    pub fn next_owned(&mut self) -> Result<Option<OwnedRecord>> {
        Ok(self.try_next()?.map(|r| r.to_owned()))
    }

    /// The sequence number of the next record this reader will return.
    pub fn position(&self) -> u64 {
        self.next_seq.max(self.skip_until)
    }

    /// Returns `Error::Discarded` if the current segment's path no longer names the file the reader has open.
    fn check_identity(&self) -> Result<()> {
        if journal::path_identity(&self.seg.path) != Some(self.seg.identity) {
            return Err(Error::Discarded {
                seq: self.position(),
            });
        }
        Ok(())
    }

    /// Finds the next record to return, moving the cursor past it (and past skipped records and seals).
    fn advance(&mut self) -> Result<Option<Found>> {
        loop {
            let Some(found) = self.next_slot()? else {
                return Ok(None);
            };
            if found.header.seq >= self.skip_until {
                return Ok(Some(found));
            }
        }
    }

    /// Reads and validates the slot at the cursor, following seals. Returns a record (cursor moved past it), or
    /// `None` at an empty slot.
    fn next_slot(&mut self) -> Result<Option<Found>> {
        loop {
            let mut attempt = 0;
            let outcome = loop {
                match self.read_slot(attempt > 0)? {
                    SlotOutcome::Invalid(_) if attempt < VALIDATION_RETRIES => attempt += 1,
                    other => break other,
                }
            };
            match outcome {
                SlotOutcome::Empty => {
                    let now = Instant::now();
                    let since = *self.waiting_since.get_or_insert(now);
                    if now.duration_since(since) >= IDENTITY_CHECK_INTERVAL {
                        self.waiting_since = Some(now);
                        self.check_identity()?;
                    }
                    return Ok(None);
                }
                SlotOutcome::Record { header, len } => {
                    self.waiting_since = None;
                    let found = Found {
                        header,
                        payload_at: self.offset + format::RECORD_HEADER_LEN as u64,
                        payload_len: len as usize - format::RECORD_HEADER_LEN,
                    };
                    let Some(next_seq) = self.next_seq.checked_add(1) else {
                        return Err(Error::corrupt(
                            &self.seg.path,
                            self.offset,
                            CorruptReason::SequenceExhausted,
                        ));
                    };
                    self.offset += format::align8(u64::from(len));
                    self.next_seq = next_seq;
                    return Ok(Some(found));
                }
                SlotOutcome::Seal => {
                    self.check_identity()?;
                    let next = match open_segment(&self.dir, self.next_seq, self.mapped) {
                        Ok(next) => next,
                        Err(Error::Io { source, .. })
                            if source.kind() == std::io::ErrorKind::NotFound =>
                        {
                            return Err(Error::corrupt(
                                &self.seg.path,
                                self.offset,
                                CorruptReason::BrokenChain(self.next_seq),
                            ));
                        }
                        Err(e) => return Err(e),
                    };
                    // Recheck after opening the successor: if recovery replaced this segment in between, the
                    // successor may belong to a different history.
                    self.check_identity()?;
                    self.seg = next;
                    self.offset = format::SEGMENT_HEADER_LEN as u64;
                    self.buf.clear();
                    self.waiting_since = None;
                }
                SlotOutcome::Invalid(reason) => {
                    self.check_identity()?;
                    return Err(Error::corrupt(&self.seg.path, self.offset, reason));
                }
            }
        }
    }

    /// One attempt at reading the slot at the cursor. `fresh` forces the `pread` path to re-read from the file.
    fn read_slot(&mut self, fresh: bool) -> Result<SlotOutcome> {
        let offset = self.offset;
        let expected = self.next_seq;
        let size = self.seg.size;
        if let Some(map) = &self.seg.map {
            let Some(word) = map.commit_word(offset) else {
                return Ok(SlotOutcome::Invalid(CorruptReason::BadLength(0)));
            };
            // Validate from an atomic copy: no reference into the mapping exists until the record has passed
            // validation (`materialize`), so a corrupted length can't make a slice cover bytes still being written.
            return Ok(classify(word, offset, size, expected, |off, len| {
                map.copy_atomic(off, len)
            }));
        }

        // pread path (ADR 0003): observe the commit word, fence, then use body bytes read after the fence.
        let buffered = !fresh
            && offset >= self.buf_start
            && offset + 4 <= self.buf_start + self.buf.len() as u64;
        let mut word = 0;
        if buffered {
            let i = (offset - self.buf_start) as usize;
            word = u32::from_le_bytes([
                self.buf[i],
                self.buf[i + 1],
                self.buf[i + 2],
                self.buf[i + 3],
            ]);
        }
        if word == 0 {
            let mut w = [0u8; 4];
            self.seg
                .file
                .read_exact_at(&mut w, offset)
                .map_err(|e| Error::io("read segment", &self.seg.path, e))?;
            word = u32::from_le_bytes(w);
            if word == 0 {
                return Ok(SlotOutcome::Empty);
            }
            fence(Ordering::Acquire);
            self.fill_buf(offset, word)?;
        } else if fresh {
            fence(Ordering::Acquire);
            self.fill_buf(offset, word)?;
        }
        let frame_end = if word == format::SEAL_MARKER {
            offset + format::SEAL_LEN as u64
        } else {
            offset.saturating_add(u64::from(word))
        };
        if frame_end > self.buf_start + self.buf.len() as u64 && frame_end <= size {
            self.fill_buf(offset, word)?;
        }
        let (buf, buf_start) = (&self.buf, self.buf_start);
        Ok(classify(word, offset, size, expected, |off, len| {
            let start = usize::try_from(off.checked_sub(buf_start)?).ok()?;
            let end = start.checked_add(usize::try_from(len).ok()?)?;
            buf.get(start..end).map(<[u8]>::to_vec)
        }))
    }

    /// Fills the `pread` buffer from `offset`: at least the frame of commit word `word`, normally a whole chunk,
    /// but never past the end of the segment.
    fn fill_buf(&mut self, offset: u64, word: u32) -> Result<()> {
        let want = if word == format::SEAL_MARKER {
            format::SEAL_LEN as u64
        } else {
            u64::from(word)
        };
        let len = want
            .max(CHUNK as u64)
            .min(self.seg.size.saturating_sub(offset));
        self.buf.resize(len as usize, 0);
        self.seg
            .file
            .read_exact_at(&mut self.buf, offset)
            .map_err(|e| Error::io("read segment", &self.seg.path, e))?;
        self.buf_start = offset;
        Ok(())
    }

    fn materialize(&self, found: Found) -> Record<'_> {
        let payload: &[u8] = match &self.seg.map {
            Some(map) => {
                // SAFETY: the payload is part of a record validated after observing its non-zero commit word;
                // `open_mapped`'s contract makes it immutable while the reader exists.
                unsafe { map.committed_bytes(found.payload_at, found.payload_len as u64) }
                    .unwrap_or(&[])
            }
            None => {
                let start = (found.payload_at - self.buf_start) as usize;
                self.buf
                    .get(start..start + found.payload_len)
                    .unwrap_or(&[])
            }
        };
        Record {
            seq: found.header.seq,
            schema_id: found.header.schema_id,
            venue_ts_ns: found.header.venue_ts_ns,
            local_ts_ns: found.header.local_ts_ns,
            payload,
        }
    }
}

fn open_segment(dir: &Path, base: u64, mapped: bool) -> Result<OpenSegment> {
    let path = journal::segment_path(dir, base);
    let file = File::open(&path).map_err(|e| Error::io("open segment", &path, e))?;
    let header = journal::read_header(&file, &path, base)?;
    let identity = journal::file_identity(&file, &path)?;
    let map = if mapped {
        let len = usize::try_from(header.segment_size).map_err(|_| {
            Error::corrupt(
                &path,
                0,
                CorruptReason::BadSegmentHeader("segment too large to map"),
            )
        })?;
        Some(MapRo::new(&file, len).map_err(|e| Error::io("map segment", &path, e))?)
    } else {
        None
    };
    Ok(OpenSegment {
        path,
        size: header.segment_size,
        file,
        identity,
        map,
    })
}

/// Validates a slot given its already observed commit word. `bytes(off, len)` returns bytes of that frame.
fn classify(
    word: u32,
    offset: u64,
    size: u64,
    expected: u64,
    bytes: impl Fn(u64, u64) -> Option<Vec<u8>>,
) -> SlotOutcome {
    if word == 0 {
        return SlotOutcome::Empty;
    }
    if word == format::SEAL_MARKER {
        // An empty segment is never sealed, and a seal at the first slot would point back at its own segment.
        if offset == format::SEGMENT_HEADER_LEN as u64
            || offset
                .checked_add(format::SEAL_LEN as u64)
                .is_none_or(|end| end > size)
        {
            return SlotOutcome::Invalid(CorruptReason::BadSeal);
        }
        return match bytes(offset + 4, 4) {
            Some(crc) if crc[..] == format::seal_crc(expected).to_le_bytes() => SlotOutcome::Seal,
            _ => SlotOutcome::Invalid(CorruptReason::BadSeal),
        };
    }
    if (word as usize) < format::RECORD_HEADER_LEN || !format::record_fits(size, offset, word) {
        return SlotOutcome::Invalid(CorruptReason::BadLength(word));
    }
    let body_len = u64::from(word) - format::COMMIT_WORD_LEN as u64;
    let Some(body) = bytes(offset + format::COMMIT_WORD_LEN as u64, body_len) else {
        return SlotOutcome::Invalid(CorruptReason::BadLength(word));
    };
    match format::decode_record_body(word, &body, expected) {
        Ok((header, _)) => SlotOutcome::Record { header, len: word },
        Err(reason) => SlotOutcome::Invalid(reason),
    }
}

/// Implements [`Backoff`].
struct Waiter {
    backoff: Backoff,
    step: u64,
    sleep: Duration,
}

impl Waiter {
    fn new(backoff: Backoff) -> Waiter {
        Waiter {
            backoff,
            step: 0,
            sleep: backoff.min_sleep,
        }
    }

    fn wait(&mut self, deadline: Option<Instant>) {
        let spins = u64::from(self.backoff.spins);
        let yields = u64::from(self.backoff.yields);
        if self.step < spins {
            std::hint::spin_loop();
        } else if self.step < spins + yields {
            std::thread::yield_now();
        } else {
            let mut d = self.sleep;
            if let Some(deadline) = deadline {
                d = d.min(deadline.saturating_duration_since(Instant::now()));
            }
            if !d.is_zero() {
                std::thread::sleep(d);
            }
            self.sleep = (self.sleep * 2)
                .min(self.backoff.max_sleep)
                .max(self.backoff.min_sleep);
        }
        self.step = self.step.saturating_add(1);
    }
}
