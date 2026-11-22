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
/// Longest single block on the wake-up region when the writer maintains it (bounded so that a writer that died, or a
/// missed wake-up, costs at most this much latency; the identity check also runs at least this often).
const NOTIFY_MAX_WAIT: Duration = Duration::from_millis(100);
/// Re-reads, back to back, of a slot that failed validation (the copy may predate the commit).
const VALIDATION_RETRIES: u32 = 3;
/// How long a reader keeps reading a slot again, with growing pauses, after its back-to-back re-reads also failed,
/// before it reports corruption (issue #43). On aarch64 Linux under memory compaction and reclaim, readers that map a
/// segment (the zero-copy reader, and an independent mapped reader) have seen a record whose commit word was already
/// visible with bytes in its next page that were not, while the file held the record intact; the view caught up
/// within microseconds, after more than `VALIDATION_RETRIES` back-to-back re-reads. The window is far longer than
/// that, and costs only a slot that stays invalid: corruption is reported this much later.
const VALIDATION_WINDOW: Duration = Duration::from_millis(50);
/// The first pause between the re-reads of [`VALIDATION_WINDOW`]; each pause doubles, up to [`VALIDATION_PAUSE_MAX`].
const VALIDATION_PAUSE_MIN: Duration = Duration::from_micros(10);
/// The longest pause between the re-reads of [`VALIDATION_WINDOW`].
const VALIDATION_PAUSE_MAX: Duration = Duration::from_millis(5);
/// Listings `open` and `seek` make when the segment they chose disappears before they can open it.
const LISTING_ATTEMPTS: u32 = 8;

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
/// `Reader` doesn't implement [`Iterator`], because returned records borrow from the reader. Loop with `while let`
/// (zero allocations), or use [`Reader::records`] for an iterator of owned copies:
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
    /// Exclusive checkpoint boundary, refreshed only when durable-only reads reach it.
    durable_next_seq: u64,
    /// The segment the marker that set `durable_next_seq` names (0 if there was no valid marker).
    durable_segment_base: u64,
    /// When a durable-only reader waiting at the checkpoint last checked a segment that the marker has moved past.
    superseded_checked: Option<Instant>,
    /// Records before this are skipped (set by `seek`).
    skip_until: u64,
    /// `pread` buffer: bytes of `seg` starting at file offset `buf_start`.
    buf: Vec<u8>,
    buf_start: u64,
    /// The journal's wake-up region, if wake-ups are enabled (ADRs 0017 and 0021).
    waker: Option<crate::notify::Waker>,
    /// When the reader started waiting at an empty slot.
    waiting_since: Option<Instant>,
    /// The record [`Reader::advance`] found last (valid only after it returned `true`). The slot reads decode a
    /// record's header straight into it, so that the per-record path passes only small values around (see
    /// [`SlotOutcome`]).
    found: Found,
}

#[derive(Debug)]
struct OpenSegment {
    base: u64,
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

/// What is at the cursor. A record's header is not part of it: the slot read decodes it into `Reader::found`.
///
/// This is deliberate. While the decoded header travelled by value through this enum, `Result<SlotOutcome>`,
/// `Option<Found>` and back, the speed of the per-record path depended on how the compiler chose to copy it: after an
/// unrelated change (#16) it copied the header through the stack in pieces that the CPU could not forward from store
/// to load, twice per record. The per-record path now passes only a tag and a length, and the checks below keep it so.
/// They catch the header (or anything as large) moving back into these values and a change of what the functions on
/// the per-record path return; they cannot catch other ways in which a change makes the compiler slower here, so a
/// change to this file, to `format.rs` or to what this path inlines or returns (`error.rs`, `mmap.rs`, `crc.rs`)
/// needs the `replay` benchmark before and after (`CONTRIBUTING.md`).
#[derive(Debug, Clone, Copy)]
enum SlotOutcome {
    Empty,
    Record { len: u32 },
    Seal,
    Invalid(CorruptReason),
}

const _: () = assert!(
    size_of::<SlotOutcome>() < size_of::<RecordHeader>(),
    "SlotOutcome must stay smaller than RecordHeader: see the comment above it (a larger CorruptReason variant \
     also trips this)"
);
const _: fn(&mut Reader, bool) -> Result<SlotOutcome> = Reader::read_slot;
const _: fn(&mut Reader) -> Result<bool> = Reader::next_slot;
const _: fn(&mut Reader) -> Result<bool> = Reader::advance;

impl Reader {
    /// Opens the journal in `dir`, positioned at the first record of the oldest segment.
    ///
    /// If retention removes the oldest segment while the reader opens it, the reader opens the oldest segment that
    /// remains; it doesn't report the removed file. A directory listing that comes back empty while retention runs
    /// (on btrfs a listing omits segments created after it started, so it can miss every segment) is listed again
    /// rather than reported as [`Error::NotFound`].
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if `dir` has no segments (for example, the writer hasn't started),
    /// [`Error::Corrupt`], [`Error::UnsupportedVersion`] if
    /// the oldest segment's header is invalid, [`Error::Io`].
    pub fn open(dir: impl AsRef<Path>, config: ReaderConfig) -> Result<Reader> {
        Reader::open_inner(dir.as_ref(), config, false)
    }

    /// Like [`Reader::open`], but returned records borrow their payload directly from the memory-mapped segment,
    /// without copying.
    ///
    /// Each record is validated (length, CRC32C, sequence number, flags) from atomic loads of the mapping before any
    /// slice into it is created. The checks, their order and the errors are those of [`Reader::open`] (ADR 0027).
    ///
    /// Segments are mapped only on x86-64 and aarch64. On other targets this opens the same reader as
    /// [`Reader::open`] (same API and results; payloads borrow from a buffer the reader owns and nothing is mapped),
    /// because the atomic loads used for validation are not documented as working on read-only memory there.
    ///
    /// # Safety
    ///
    /// While the returned reader exists, no process other than a tickertape [`Writer`](crate::Writer) may modify
    /// the journal's segment files (writing into them, truncating them, or replacing their contents through other
    /// tools). If a mapped file is truncated, the process receives `SIGBUS`.
    ///
    /// What follows holds for an intact publication chain, where every commit word the reader follows is one a
    /// writer published. A tickertape writer then never modifies committed bytes, including during crash recovery,
    /// which in place only zeroes bytes after an empty slot (never committed, on an intact chain) and writes a seal
    /// there, and otherwise replaces or moves whole files (`docs/format.md` → Recovery). A returned
    /// payload is the bytes that were validated only because nothing modifies them afterwards.
    ///
    /// Known limitation: a commit word damaged on storage breaks the chain, and then a mapped load can touch bytes a
    /// writer is storing to without synchronisation. If the writer runs in the same process, that is a data race,
    /// undefined behaviour in Rust's terms, before any CRC runs; for a writer in another process the language says
    /// nothing. Two cases (ADR 0027 → "Which loads can race"):
    ///
    /// - The length in an earlier record's commit word is enlarged while no writer runs, and a writer then appends
    ///   after that record: validating it loads bytes the writer is storing.
    /// - The commit word of a published record not yet covered by a checkpoint is zeroed, and the next writer's
    ///   recovery cannot tell that from a torn append (damage after which nothing validates, such as the newest
    ///   record's commit word zeroed; `docs/crash-contract.md`, C11 (3), has the cases). It then takes the slot for
    ///   the end of the journal and zeroes that record and the published records after it in place, under a live
    ///   zero-copy reader. A payload the caller still holds then changes, and the reader's later loads meet the new
    ///   writer's stores. (When recovery finds evidence of the records, it usually replaces the segment by a new
    ///   file instead, and mappings of the old file do not change. ADR 0035.)
    ///
    /// That such loads return some mixture of old and new bytes, which the CRC then rejects, is an assumption about
    /// the implementation on the supported targets, not a guarantee. Earlier releases had the same exposure.
    ///
    /// # Errors
    ///
    /// As for [`Reader::open`].
    pub unsafe fn open_mapped(dir: impl AsRef<Path>, config: ReaderConfig) -> Result<Reader> {
        Reader::open_inner(dir.as_ref(), config, crate::mmap::MAPPED_READS)
    }

    fn open_inner(dir: &Path, config: ReaderConfig, mapped: bool) -> Result<Reader> {
        let (base, seg) = open_listed(dir, mapped, 0, |bases| match bases.oldest {
            Some(base) => Ok(base),
            None => Err(Error::NotFound {
                dir: dir.to_path_buf(),
            }),
        })?;
        let waker = config.notify.then(|| crate::notify::Waker::new(dir));
        Ok(Reader {
            dir: dir.to_path_buf(),
            config,
            mapped,
            seg,
            offset: format::SEGMENT_HEADER_LEN as u64,
            next_seq: base,
            durable_next_seq: 0,
            durable_segment_base: 0,
            superseded_checked: None,
            skip_until: base,
            buf: Vec::new(),
            buf_start: 0,
            waker,
            waiting_since: None,
            found: Found {
                header: RecordHeader {
                    seq: 0,
                    schema_id: 0,
                    venue_ts_ns: 0,
                    local_ts_ns: 0,
                },
                payload_at: 0,
                payload_len: 0,
            },
        })
    }

    /// Positions the reader so that the next record returned is `seq`.
    ///
    /// If `seq` hasn't been written yet, the reader waits for it: [`Reader::try_next`] returns `None` until the
    /// writer gets there, and records before `seq` are skipped. Every record on the way is validated, so the reader
    /// only lands on positions along the journal's current chain.
    ///
    /// Retention may remove the segment holding `seq` while `seek` runs (between listing the segments and opening
    /// the chosen one). `seek` then lists the segments again, so it returns [`Error::SeqUnavailable`] with the new
    /// oldest sequence number, never an I/O error for the removed file. A segment removed just after `seek`
    /// returns is still read, and the reader returns [`Error::Discarded`] at its end, as for any removed segment; a
    /// durable-only reader may get it earlier, when the checkpoint moves past its position (ADR 0034).
    ///
    /// # Errors
    ///
    /// [`Error::SeqUnavailable`] if `seq` is older than the oldest segment, including when retention removes it
    /// during the call;
    /// [`Error::Corrupt`] or [`Error::Io`] while locating it. An I/O "not found" error for a segment that `seek`
    /// listed is returned only if the chosen segment disappeared on each of 8 listings in a row.
    pub fn seek(&mut self, seq: u64) -> Result<()> {
        let (base, seg) = open_listed(&self.dir, self.mapped, seq, |bases| {
            let Some(first) = bases.oldest else {
                return Err(Error::NotFound {
                    dir: self.dir.clone(),
                });
            };
            bases.floor.ok_or(Error::SeqUnavailable {
                requested: seq,
                first,
            })
        })?;
        self.seg = seg;
        self.offset = format::SEGMENT_HEADER_LEN as u64;
        self.next_seq = base;
        self.skip_until = seq;
        self.buf.clear();
        self.waiting_since = None;
        self.superseded_checked = None;
        Ok(())
    }

    /// Returns the next record if one is committed, without waiting for one.
    ///
    /// `Ok(None)` means there is no new record yet. Follows seals into the next segment transparently.
    /// One case does wait: a committed slot that fails validation is read again for at least 50 ms before it is
    /// reported, because a reader's view of a record just committed can lag (`docs/crash-contract.md` → Known risks
    /// and limits). A call that ends in [`Error::Corrupt`], or in [`Error::Discarded`] after a repair, can therefore
    /// take that long, and [`Reader::next_timeout`] can pass its timeout by the same amount.
    /// With [`ReaderConfig::durable_only`] enabled, records wait at the cursor until covered by a checkpoint;
    /// a missing or invalid checkpoint marker covers no records. A marker describes the journal's current files, so a
    /// durable-only reader applies a marker that moves past its cursor only after confirming that its segment is still
    /// in place; if recovery replaced it or retention removed it, the reader returns [`Error::Discarded`] then, instead
    /// of reading on in the file it has open (ADR 0034). If it is in place, the reader reads the records the marker
    /// newly covers from the file's current bytes, so records that recovery removed in place are never returned
    /// (crash contract C13 states the exception: damage that also destroys the checkpoint marker).
    /// A visible reader can't notice such an in-place repair (storage damage that emptied a commit word; crash
    /// contract C11): it may return records it had already read, then continue in the new history, wait, or report
    /// corruption at a position that no longer starts a record; [`Reader::seek`] to its position puts it back on the
    /// current chain.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`] if the bytes at the cursor fail validation. The cursor doesn't move,
    /// so calling again retries the same position (which may succeed after a writer recovery).
    /// [`Error::Discarded`] if writer recovery or retention replaced or removed the segment at the cursor
    /// (including when retention removed both it and the next segment while the reader was moving between them);
    /// call [`Reader::seek`] to continue on the current chain. A removal is never reported as corruption or as an
    /// I/O "not found" error.
    /// [`Error::Corrupt`] with [`CorruptReason::BrokenChain`] if the segment at the cursor is sealed and still in
    /// place but the next segment is missing. When the next segment is not found, the reader first reads the seal
    /// again from the file: if the seal is gone (a repair in place can remove one the reader had read or buffered,
    /// crash contract C11), it reads the slot as it is now; if it is still there, it tries the next segment once
    /// more and reports the broken chain only if that misses too. A later call tries again.
    /// [`Error::Io`] if the next segment can't be opened, if checking whether the segment is still in place fails
    /// other than with "not found" (only that counts as a removal), or (with kind
    /// [`OutOfMemory`](std::io::ErrorKind::OutOfMemory)) if the read buffer for the frame at the cursor can't be
    /// allocated.
    pub fn try_next(&mut self) -> Result<Option<Record<'_>>> {
        if self.advance()? {
            Ok(Some(self.materialize()))
        } else {
            Ok(None)
        }
    }

    /// Returns the next record, polling with the configured [`Backoff`] until one is committed or
    /// `timeout` passes (`None`, or a timeout too large to represent: wait forever). `Ok(None)` means the timeout
    /// passed. With [`ReaderConfig::durable_only`] enabled, also waits for checkpoint coverage, refreshing the marker
    /// at most once per poll.
    ///
    /// # Errors
    ///
    /// As for [`Reader::try_next`].
    pub fn next_timeout(&mut self, timeout: Option<Duration>) -> Result<Option<Record<'_>>> {
        let deadline = timeout.and_then(|t| Instant::now().checked_add(t));
        let mut waiter = Waiter::new(self.config.backoff);
        loop {
            if self.advance()? {
                return Ok(Some(self.materialize()));
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(None);
            }
            if waiter.in_blocking_phase() {
                if let Some(blocked) = self.block(deadline) {
                    if blocked? {
                        return Ok(Some(self.materialize()));
                    }
                    continue;
                }
            }
            // Wake up for the next refresh of the wake-up record too, so a writer that starts while this reader
            // polls (with a long `max_sleep`) is noticed within the refresh interval.
            let refresh = self.waker.as_ref().map(crate::notify::Waker::refresh_due);
            waiter.wait(earliest(deadline, refresh));
        }
    }

    /// Blocks on the journal's wake-up region, if there is one: `None` if the reader should poll instead.
    fn block(&mut self, deadline: Option<Instant>) -> Option<Result<bool>> {
        let waker = self.waker.as_mut()?;
        waker.refresh_if_due();
        // Never block past the next refresh: a writer that restarted can only wake readers on its new region.
        let until = earliest(deadline, Some(waker.refresh_due()));
        let (nonce, notify) = waker.take()?;
        let (result, supported) = self.block_on(&notify, until);
        if supported {
            if let Some(waker) = self.waker.as_mut() {
                waker.put((nonce, notify));
            }
        } else {
            // The platform's wait call doesn't work here: poll from now on.
            self.waker = None;
        }
        Some(result)
    }

    /// Blocks on the wake-up region until the writer publishes something or a timeout passes (event-count protocol,
    /// ADR 0017): register as a waiter, check once more, then wait for the event counter to change. Returns whether
    /// the platform's wait call is supported; if not, this call slept instead (bounded like a poll).
    fn block_on(
        &mut self,
        notify: &crate::mmap::Notify,
        deadline: Option<Instant>,
    ) -> (Result<bool>, bool) {
        let seen = notify.begin_wait();
        let found = self.advance();
        if !matches!(found, Ok(false)) {
            notify.end_wait();
            return (found, true);
        }
        let cap = if notify.writer_notifies() {
            NOTIFY_MAX_WAIT
        } else {
            self.config.backoff.max_sleep
        };
        let wait = deadline.map_or(cap, |d| {
            cap.min(d.saturating_duration_since(Instant::now()))
        });
        let mut supported = true;
        if !wait.is_zero() && notify.wait(seen, wait) == crate::mmap::WaitOutcome::Unsupported {
            supported = false;
            std::thread::sleep(wait.min(self.config.backoff.max_sleep));
        }
        notify.end_wait();
        (Ok(false), supported)
    }

    /// Like [`Reader::try_next`], but copies the record out of the reader.
    ///
    /// # Errors
    ///
    /// As for [`Reader::try_next`].
    pub fn next_owned(&mut self) -> Result<Option<OwnedRecord>> {
        Ok(self.try_next()?.map(|r| r.to_owned()))
    }

    /// An iterator over the records committed so far, copied out of the reader, starting at the current position.
    ///
    /// It ends at the first slot with no committed record (it doesn't wait for more; call `records()` again later
    /// or use [`Reader::next_timeout`] to follow live). After an error it yields that error once and ends; the
    /// reader's position is where [`Reader::try_next`] left it, so you can handle the error and continue.
    ///
    /// ```no_run
    /// # use tickertape::{Reader, ReaderConfig};
    /// # fn main() -> tickertape::Result<()> {
    /// let mut reader = Reader::open("/tmp/journal", ReaderConfig::default())?;
    /// let trades: Vec<_> = reader
    ///     .records()
    ///     .filter(|r| r.as_ref().map_or(true, |r| r.schema_id == 1))
    ///     .collect::<tickertape::Result<_>>()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn records(&mut self) -> Records<'_> {
        Records {
            reader: self,
            done: false,
        }
    }

    /// The sequence number of the next record this reader will return.
    pub fn position(&self) -> u64 {
        self.next_seq.max(self.skip_until)
    }

    /// Returns `Error::Discarded` if the current segment's path no longer names the file the reader has open, and
    /// `Error::Io` if that can't be checked (a `stat` failing other than with "not found").
    fn check_identity(&self) -> Result<()> {
        if journal::path_identity(&self.seg.path)? != Some(self.seg.identity) {
            return Err(Error::Discarded {
                seq: self.position(),
            });
        }
        Ok(())
    }

    /// Finds the next record to return, moving the cursor past it (and past skipped records and seals). Returns
    /// `true` with the record in `self.found`, or `false` if there is none yet.
    fn advance(&mut self) -> Result<bool> {
        let mut refreshed = false;
        loop {
            if self.config.durable_only
                && self.next_seq >= self.durable_next_seq
                && !self.durable_covers_cursor(&mut refreshed)?
            {
                return Ok(false);
            }
            if !self.next_slot()? {
                return Ok(false);
            }
            if self.found.header.seq >= self.skip_until {
                return Ok(true);
            }
        }
    }

    /// For a durable-only reader whose cursor is at the checkpoint boundary it knows: refreshes the marker (once per
    /// [`Reader::advance`] call, tracked in `refreshed`) and says whether it now covers the cursor. If not, and the
    /// marker names a later segment, see [`Reader::leave_superseded_segment`].
    #[inline(never)]
    fn durable_covers_cursor(&mut self, refreshed: &mut bool) -> Result<bool> {
        if !*refreshed {
            self.refresh_checkpoint()?;
            *refreshed = true;
        }
        if self.next_seq >= self.durable_next_seq {
            if self.durable_segment_base > self.seg.base {
                self.leave_superseded_segment()?;
            }
            return Ok(false);
        }
        Ok(true)
    }

    /// Called when a durable-only reader is waiting at the checkpoint and the marker names a later segment than the
    /// one it holds. Every record of the held segment is then covered, so the reader is at its seal (if the file is
    /// still the current one), and the marker can't move past the cursor while the writer appends nothing to the new
    /// segment: without this, the reader would never check its segment, and one that retention removed would leave it
    /// returning `Ok(None)` for as long as the writer stays idle, where a visible reader returns `Error::Discarded`
    /// (crash contract C14). So it does what a visible reader does at that slot: at a seal it checks the segment's
    /// identity and moves to the successor, without returning anything (the successor is opened after the marker was
    /// read, so ADR 0034 needs no further check for it); a removed or replaced segment gives `Error::Discarded`, which
    /// is the result when retention removed it: the reader can't tell a retention removal from a repair (ADR 0034,
    /// option 5), and nothing is lost, since `seek` to the cursor lands on the successor's first record. At any other
    /// slot (the held file isn't the current one) it checks the identity only.
    ///
    /// Throttled to once per [`IDENTITY_CHECK_INTERVAL`], the interval at which a visible reader waiting at an empty
    /// slot checks its segment: a removal is noticed as quickly as a visible reader notices it. At a seal the first
    /// check moves the reader on, so a reader keeping up with the writer makes one per roll; it repeats (at most ten
    /// `stat`s a second) only while the held file shows no seal at the cursor, and a reader whose marker names its own
    /// segment never makes it.
    fn leave_superseded_segment(&mut self) -> Result<()> {
        let now = Instant::now();
        if self
            .superseded_checked
            .is_some_and(|at| now.duration_since(at) < IDENTITY_CHECK_INTERVAL)
        {
            return Ok(());
        }
        self.superseded_checked = Some(now);
        // `fresh`: the safe reader reads the slot from the file, not its buffer. The buffer can hold a record past the
        // checkpoint that recovery has since zeroed in place and that the seal now covers; probed from the buffer, it
        // would hide the seal on every call. The mapping shows the current bytes anyway.
        match self.read_slot(true)? {
            SlotOutcome::Seal => self.follow_seal(),
            _ => self.check_identity(),
        }
    }

    /// Read the marker through ordinary file I/O (never a mapping); invalid or missing means no coverage.
    ///
    /// A marker speaks about the journal's current files, not about the file this reader has open, which recovery
    /// may have replaced (or retention removed) since. So a marker that moves past the cursor is applied only after
    /// confirming, *after* reading it, that the segment's path still names the open file; otherwise this returns
    /// `Error::Discarded` without applying it. Read first, then check: the check then proves the open file was current
    /// at a moment after the marker was written, and recovery keeps every record a marker covers, byte for byte, in
    /// the current files from then on. Checking first would let a repair and the new writer's checkpoint land between
    /// the check and the read, applying the new marker to the old file. The open file's descriptor keeps its inode
    /// allocated, so no later file can have its identity.
    ///
    /// When the check passes, the `pread` buffer is discarded, so every record the marker newly covers is read from
    /// the file after the check. Bytes past the old marker that were buffered before may predate a repair that kept
    /// the file (recovery zeroing, in place, records after the checkpoint whose commit word storage damage emptied),
    /// which no identity check can see. Bytes before a marker never change in place, so records below the new marker
    /// read now are the covered ones, and the cursor (never past a marker the reader applied) is a record boundary of
    /// the current history. The mapping needs nothing: it shows the current bytes. ADR 0034 has the full argument.
    fn refresh_checkpoint(&mut self) -> Result<()> {
        crate::fault::hook(crate::fault::Point::ReaderCheckpoint);
        let (durable, segment_base) = crate::inspect::read_checkpoint(&self.dir)?
            .map_or((0, 0), |c| (c.next_seq, c.segment_base_seq));
        if durable > self.next_seq {
            crate::fault::hook(crate::fault::Point::ReaderCheckpointIdentity);
            self.check_identity()?;
            // Bytes buffered before the marker was read may predate an in-place repair after the old marker.
            self.buf.clear();
        }
        self.durable_next_seq = durable;
        self.durable_segment_base = segment_base;
        Ok(())
    }

    /// Reads and validates the slot at the cursor, following seals. Returns `true` with the record in `self.found`
    /// (cursor moved past it), or `false` at an empty slot.
    ///
    /// A slot that fails validation is read again from the file, for at least [`VALIDATION_WINDOW`], before it is
    /// reported ([`Reader::read_slot_again`]). The record path is kept short: the other outcomes are handled in functions that are never inlined.
    /// (They are not marked `#[cold]`: that moved them to the end of the code and, on the machine where this was
    /// measured, made the unrelated append path slower by changing where its code landed.)
    fn next_slot(&mut self) -> Result<bool> {
        loop {
            let read = self.read_slot(false)?;
            #[cfg(test)]
            let read = tests::scripted(read);
            let outcome = match read {
                SlotOutcome::Record { len } => return self.step_past_record(len),
                SlotOutcome::Invalid(_) => self.read_slot_again()?,
                other => other,
            };
            match outcome {
                SlotOutcome::Record { len } => return self.step_past_record(len),
                SlotOutcome::Empty => {
                    self.at_empty_slot()?;
                    return Ok(false);
                }
                SlotOutcome::Seal => self.follow_seal()?,
                SlotOutcome::Invalid(reason) => return Err(self.invalid_slot(reason)),
            }
        }
    }

    /// The retries of a slot whose first read failed validation: fresh reads until one gives something other than
    /// `Invalid`, first [`VALIDATION_RETRIES`] back to back, then with growing pauses until [`VALIDATION_WINDOW`] has
    /// passed since the last of those (the last outcome is returned either way, and the slot is read at least once
    /// after the window has passed).
    #[inline(never)]
    fn read_slot_again(&mut self) -> Result<SlotOutcome> {
        let mut attempt = 1;
        let mut window: Option<(Instant, Duration)> = None;
        loop {
            let read = self.read_slot(true)?;
            #[cfg(test)]
            let read = tests::scripted(read);
            if !matches!(read, SlotOutcome::Invalid(_)) {
                return Ok(read);
            }
            if attempt < VALIDATION_RETRIES {
                attempt += 1;
                continue;
            }
            let now = Instant::now();
            let (deadline, pause) =
                window.get_or_insert((now + VALIDATION_WINDOW, VALIDATION_PAUSE_MIN));
            if now >= *deadline {
                return Ok(read);
            }
            std::thread::sleep((*pause).min(*deadline - now));
            *pause = (*pause * 2).min(VALIDATION_PAUSE_MAX);
        }
    }

    /// Completes a record that the slot read decoded into `self.found`: records where its payload is and moves the
    /// cursor past it. Always returns `Ok(true)` unless the sequence numbers are exhausted.
    #[inline(always)]
    fn step_past_record(&mut self, len: u32) -> Result<bool> {
        self.waiting_since = None;
        let Some(next_seq) = self.next_seq.checked_add(1) else {
            return Err(self.sequence_exhausted());
        };
        self.found.payload_at = self.offset + format::RECORD_HEADER_LEN as u64;
        self.found.payload_len = len as usize - format::RECORD_HEADER_LEN;
        self.offset += format::align8(u64::from(len));
        self.next_seq = next_seq;
        Ok(true)
    }

    #[inline(never)]
    fn sequence_exhausted(&self) -> Error {
        Error::corrupt(
            &self.seg.path,
            self.offset,
            CorruptReason::SequenceExhausted,
        )
    }

    /// At an empty slot: checks the segment's identity once per [`IDENTITY_CHECK_INTERVAL`] of waiting there.
    #[inline(never)]
    fn at_empty_slot(&mut self) -> Result<()> {
        let now = Instant::now();
        let since = *self.waiting_since.get_or_insert(now);
        if now.duration_since(since) >= IDENTITY_CHECK_INTERVAL {
            self.waiting_since = Some(now);
            self.check_identity()?;
        }
        Ok(())
    }

    /// The error for a slot that still fails validation after the retries (`Error::Discarded` if the segment was
    /// replaced or removed).
    #[inline(never)]
    fn invalid_slot(&self, reason: CorruptReason) -> Error {
        match self.check_identity() {
            Err(e) => e,
            Ok(()) => Error::corrupt(&self.seg.path, self.offset, reason),
        }
    }

    /// Moves from the seal at the cursor to the next segment, checking that the segment at the cursor is still in place.
    ///
    /// Returns `Ok(())` without moving if the successor is missing and the slot at the cursor no longer holds the
    /// seal: `next_slot` then reads the slot again, and `leave_superseded_segment` lets its caller report that
    /// nothing is readable yet.
    fn follow_seal(&mut self) -> Result<()> {
        self.check_identity()?;
        crate::fault::hook(crate::fault::Point::ReaderSeal);
        let next = match open_segment(&self.dir, self.next_seq, self.mapped) {
            Ok(next) => next,
            Err(e) if e.is_not_found() => {
                // Retention removes the oldest segment first, so a successor it removed means this segment is gone
                // too: that is discarded history.
                self.check_identity()?;
                // The seal may no longer be in this file, which kept its identity. The `pread` buffer can hold a seal
                // that storage damage has since zeroed, after which recovery left this file in place and quarantined
                // the successor (the whole seal zeroed in front of a successor that no longer opens, or a zeroed
                // stretch from a record through the seal: crash contract C11 (3), ADR 0035 → Damage around a seal); a
                // mapped reader can load the seal just before the damage. Read the slot again from the
                // file: if the seal is gone, return to the caller, which reads that slot as it is now. The re-read
                // skips the buffer anyway; the buffer is dropped for the caller's sake, because a re-read that finds
                // an empty word does not refill it, and the caller's next read would take the stale seal from it
                // again and come back here without end.
                self.buf.clear();
                crate::fault::hook(crate::fault::Point::ReaderSealReread);
                if !matches!(self.read_slot(true)?, SlotOutcome::Seal) {
                    return Ok(());
                }
                // The seal is in the file now. A live writer stores a seal only after linking the successor, so a
                // successor linked since the first attempt (a writer that recovered in between and rolled again)
                // opens now. Missing while this segment is still in place, it is a broken chain. (One report can
                // still be transient: a recovery that replaces a successor whose header is damaged keeps the seal,
                // moves that successor to quarantine and then creates a new one; the next call follows it.)
                match open_segment(&self.dir, self.next_seq, self.mapped) {
                    Ok(next) => next,
                    Err(e) if e.is_not_found() => {
                        self.check_identity()?;
                        return Err(Error::corrupt(
                            &self.seg.path,
                            self.offset,
                            CorruptReason::BrokenChain(self.next_seq),
                        ));
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        };
        // Recheck after opening the successor: if recovery replaced this segment in between, the successor may belong
        // to a different history.
        self.check_identity()?;
        self.seg = next;
        self.offset = format::SEGMENT_HEADER_LEN as u64;
        self.buf.clear();
        self.waiting_since = None;
        self.superseded_checked = None;
        Ok(())
    }

    /// One attempt at reading the slot at the cursor. `fresh` forces the `pread` path to re-read from the file. A
    /// record's header is decoded into `self.found.header` (also by reads whose record the caller then ignores).
    fn read_slot(&mut self, fresh: bool) -> Result<SlotOutcome> {
        let offset = self.offset;
        let expected = self.next_seq;
        let size = self.seg.size;
        if let Some(map) = &self.seg.map {
            let Some(word) = map.commit_word(offset) else {
                return Ok(SlotOutcome::Invalid(CorruptReason::BadLength(0)));
            };
            // Validate straight from `Relaxed` atomic loads of the mapping (ADR 0027): no `&[u8]` or other reference
            // asserting immutability exists until the record has passed validation (`materialize`), and nothing is
            // copied. A damaged length is normally rejected by the bounds checks and the CRC before any slice
            // exists; validation is not proof, and ADR 0027 → "Which loads can race" says what a damaged commit
            // word can lead to.
            return Ok(read_mapped(
                map,
                word,
                offset,
                size,
                expected,
                &mut self.found.header,
            ));
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
        // Validate in place in the pread buffer (no copy, no allocation).
        let pending = classify(word, offset, size);
        let len = pending_len(pending);
        let bytes = (offset + 4)
            .checked_sub(self.buf_start)
            .and_then(|start| usize::try_from(start).ok())
            .and_then(|start| self.buf.get(start..start.checked_add(len as usize)?));
        Ok(finish(
            pending,
            word,
            expected,
            bytes,
            &mut self.found.header,
        ))
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
        resize_buf(&mut self.buf, len)
            .map_err(|e| Error::io("allocate read buffer", &self.seg.path, e))?;
        if let Err(e) = self.seg.file.read_exact_at(&mut self.buf, offset) {
            // The buffer now holds whatever was read, under the previous `buf_start`: drop it, so that the next
            // call reads from the file.
            self.buf.clear();
            return Err(Error::io("read segment", &self.seg.path, e));
        }
        self.buf_start = offset;
        Ok(())
    }

    /// The record [`Reader::advance`] just found, borrowing its payload from the mapping or the buffer.
    fn materialize(&self) -> Record<'_> {
        let found = &self.found;
        let payload: &[u8] = match &self.seg.map {
            Some(map) => {
                // SAFETY: the payload is part of a record validated after observing its non-zero commit word;
                // on an intact publication chain `open_mapped`'s contract makes it immutable while the reader
                // exists (ADR 0027 → "Which loads can race" describes both cases after storage damage).
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

impl Reader {
    /// For [`Reader::cursor`] and reader beacons (`status.rs`): the cursor and the segment file it is in.
    pub(crate) fn cursor_parts(&self) -> crate::status::CursorParts<'_> {
        crate::status::CursorParts {
            slot_seq: self.next_seq,
            skip_until: self.skip_until,
            path: &self.seg.path,
            offset: self.offset,
            file: &self.seg.file,
            identity: self.seg.identity,
            durable_only: self.config.durable_only,
        }
    }
}

/// Lists the segments, keeping only the oldest base and the newest base `<= seq` ([`journal::find_bases`]), lets
/// `choose` pick one, and opens it. If the chosen segment has disappeared by the time it is opened (retention or
/// archiving removed it, or writer recovery quarantined it), lists again, so that `choose` sees the journal as it is
/// now (and likewise if the listing was empty). Retention removes the oldest segments first, so
/// the second listing already reflects a removal; after [`LISTING_ATTEMPTS`] listings in a row whose chosen segment
/// vanished, the last "not found" error is returned.
fn open_listed(
    dir: &Path,
    mapped: bool,
    seq: u64,
    choose: impl Fn(journal::Bases) -> Result<u64>,
) -> Result<(u64, OpenSegment)> {
    let mut attempts = 1;
    loop {
        let bases = journal::find_bases(dir, seq)?;
        // On a live journal a directory read can come back empty: btrfs returns only entries that existed when the
        // directory was opened and still exist when it is read, so retention removing them all while new segments
        // are created leaves none. List again.
        if bases.oldest.is_none() && attempts < LISTING_ATTEMPTS {
            attempts += 1;
            continue;
        }
        let base = choose(bases)?;
        crate::fault::hook(crate::fault::Point::ReaderOpen);
        match open_segment(dir, base, mapped) {
            Err(e) if e.is_not_found() && attempts < LISTING_ATTEMPTS => attempts += 1,
            result => return result.map(|seg| (base, seg)),
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
        base,
        path,
        size: header.segment_size,
        file,
        identity,
        map,
    })
}

/// Resizes the `pread` buffer to `len` bytes without panicking: a frame longer than the address space allows (on a
/// 32-bit target, a valid record of more than `isize::MAX` bytes in a large segment) is reported as an
/// [`std::io::ErrorKind::OutOfMemory`] error, leaving `buf` unchanged, instead of aborting in `Vec::resize`.
fn resize_buf(buf: &mut Vec<u8>, len: u64) -> std::io::Result<()> {
    let out_of_memory = || std::io::Error::from(std::io::ErrorKind::OutOfMemory);
    let len = usize::try_from(len).map_err(|_| out_of_memory())?;
    if let Some(additional) = len.checked_sub(buf.len()) {
        buf.try_reserve_exact(additional)
            .map_err(|_| out_of_memory())?;
    }
    buf.resize(len, 0);
    Ok(())
}

/// Finishes classifying a slot of a mapped segment, given its observed commit word (the zero-copy reader, ADR 0027).
/// Every byte is read with a `Relaxed` atomic load, each at most once; the results are those [`finish`] gives for
/// the same bytes. A record's header goes to `header`.
fn read_mapped(
    map: &MapRo,
    word: u32,
    offset: u64,
    size: u64,
    expected: u64,
    header: &mut RecordHeader,
) -> SlotOutcome {
    match classify(word, offset, size) {
        Pending::Done(outcome) => outcome,
        Pending::Seal => match offset.checked_add(4).and_then(|at| map.load_u32(at)) {
            Some(crc) if crc == format::seal_crc(expected) => SlotOutcome::Seal,
            _ => SlotOutcome::Invalid(CorruptReason::BadSeal),
        },
        Pending::Record { .. } => {
            // The CRC field at P+4, and the words of [P+8, P+align8(len)): in bounds, since `classify` checked
            // that the record fits.
            let crc = offset.checked_add(4).and_then(|at| map.load_u32(at));
            let words = offset
                .checked_add(8)
                .and_then(|at| map.words(at, (format::align8(u64::from(word)) - 8) / 8));
            match (crc, words) {
                (Some(crc), Some(words)) => {
                    match format::decode_record_words(word, crc, words, expected) {
                        Ok(decoded) => {
                            *header = decoded;
                            SlotOutcome::Record { len: word }
                        }
                        Err(reason) => SlotOutcome::Invalid(reason),
                    }
                }
                _ => SlotOutcome::Invalid(CorruptReason::BadLength(word)),
            }
        }
    }
}

/// What the commit word alone says about a slot; the bytes it covers are checked by [`finish`].
#[derive(Debug, Clone, Copy)]
enum Pending {
    /// Fully classified already.
    Done(SlotOutcome),
    /// A seal: check the 4-byte CRC at `offset + 4`.
    Seal,
    /// A record: validate the body `[offset + 4, offset + word)`.
    Record { body_len: u64 },
}

/// Classifies a slot from its (already observed) commit word, checking only bounds.
fn classify(word: u32, offset: u64, size: u64) -> Pending {
    if word == 0 {
        return Pending::Done(SlotOutcome::Empty);
    }
    if word == format::SEAL_MARKER {
        // An empty segment is never sealed, and a seal at the first slot would point back at its own segment.
        if offset == format::SEGMENT_HEADER_LEN as u64
            || offset
                .checked_add(format::SEAL_LEN as u64)
                .is_none_or(|end| end > size)
        {
            return Pending::Done(SlotOutcome::Invalid(CorruptReason::BadSeal));
        }
        return Pending::Seal;
    }
    if (word as usize) < format::RECORD_HEADER_LEN || !format::record_fits(size, offset, word) {
        return Pending::Done(SlotOutcome::Invalid(CorruptReason::BadLength(word)));
    }
    Pending::Record {
        body_len: u64::from(word) - format::COMMIT_WORD_LEN as u64,
    }
}

/// The length of the bytes after the commit word that [`finish`] needs for a pending slot.
fn pending_len(pending: Pending) -> u64 {
    match pending {
        Pending::Done(_) => 0,
        Pending::Seal => 4,
        Pending::Record { body_len } => body_len,
    }
}

/// Finishes classifying a slot, given the bytes after its commit word (`None` if they couldn't be read). A record's
/// header goes to `header`.
fn finish(
    pending: Pending,
    word: u32,
    expected: u64,
    bytes: Option<&[u8]>,
    header: &mut RecordHeader,
) -> SlotOutcome {
    match (pending, bytes) {
        (Pending::Done(outcome), _) => outcome,
        (Pending::Seal, Some(crc)) if crc == format::seal_crc(expected).to_le_bytes() => {
            SlotOutcome::Seal
        }
        (Pending::Seal, _) => SlotOutcome::Invalid(CorruptReason::BadSeal),
        (Pending::Record { .. }, None) => SlotOutcome::Invalid(CorruptReason::BadLength(word)),
        (Pending::Record { .. }, Some(body)) => {
            match format::decode_record_body(word, body, expected) {
                Ok((decoded, _)) => {
                    *header = decoded;
                    SlotOutcome::Record { len: word }
                }
                Err(reason) => SlotOutcome::Invalid(reason),
            }
        }
    }
}

/// Iterator returned by [`Reader::records`]: owned records committed so far.
#[derive(Debug)]
pub struct Records<'r> {
    reader: &'r mut Reader,
    done: bool,
}

impl Iterator for Records<'_> {
    type Item = Result<OwnedRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.reader.next_owned() {
            Ok(Some(record)) => Some(Ok(record)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
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

    /// True once the spin and yield phases are over (the point where a reader sleeps or blocks).
    fn in_blocking_phase(&self) -> bool {
        self.step >= u64::from(self.backoff.spins) + u64::from(self.backoff.yields)
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
            self.sleep = self
                .sleep
                .saturating_mul(2)
                .min(self.backoff.max_sleep)
                .max(self.backoff.min_sleep);
        }
        self.step = self.step.saturating_add(1);
    }
}

/// The earlier of two optional instants (`None` means never).
fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod wake_tests {
    use super::*;
    use crate::mmap::test_hooks::{FORCE_UNSUPPORTED, LOCK, WAITS};
    use crate::{Writer, WriterConfig};

    fn blocking(sleep: Duration) -> Backoff {
        Backoff {
            spins: 0,
            yields: 0,
            min_sleep: sleep,
            max_sleep: sleep,
        }
    }

    /// Resets the forced failure even if the test panics.
    struct Unforce;
    impl Drop for Unforce {
        fn drop(&mut self) {
            FORCE_UNSUPPORTED.store(false, Ordering::SeqCst);
        }
    }

    #[test]
    fn idle_waits_block_instead_of_spinning() {
        let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let _w = Writer::open(dir.path(), WriterConfig::default().segment_size(1 << 20)).unwrap();
        let mut r = Reader::open(
            dir.path(),
            ReaderConfig::default().backoff(blocking(Duration::from_secs(10))),
        )
        .unwrap();
        let before = WAITS.load(Ordering::SeqCst);
        let started = Instant::now();
        assert!(
            r.next_timeout(Some(Duration::from_millis(350)))
                .unwrap()
                .is_none()
        );
        assert!(started.elapsed() >= Duration::from_millis(350));
        // Each wait lasts up to the 100 ms cap (and the 100 ms record refresh): a handful, not a busy loop.
        let waits = WAITS.load(Ordering::SeqCst) - before;
        assert!((2..=20).contains(&waits), "{waits} waits in 350 ms");
    }

    #[test]
    fn unsupported_waits_fall_back_to_sleeping() {
        let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let _w = Writer::open(dir.path(), WriterConfig::default().segment_size(1 << 20)).unwrap();
        let mut r = Reader::open(
            dir.path(),
            ReaderConfig::default().backoff(blocking(Duration::from_millis(20))),
        )
        .unwrap();
        assert!(r.waker.is_some());
        let _unforce = Unforce;
        FORCE_UNSUPPORTED.store(true, Ordering::SeqCst);
        let before = WAITS.load(Ordering::SeqCst);
        let started = Instant::now();
        assert!(
            r.next_timeout(Some(Duration::from_millis(300)))
                .unwrap()
                .is_none()
        );
        assert!(started.elapsed() >= Duration::from_millis(300));
        // One failed wait turns wake-ups off; the rest of the time the reader sleeps in 20 ms steps.
        assert_eq!(WAITS.load(Ordering::SeqCst) - before, 1);
        assert!(
            r.waker.is_none(),
            "wake-ups still on after an unsupported wait"
        );
    }

    #[test]
    fn huge_timeouts_and_sleeps_do_not_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let mut w =
            Writer::open(dir.path(), WriterConfig::default().segment_size(1 << 20)).unwrap();
        w.append(&crate::NewRecord {
            schema_id: 0,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: b"x",
        })
        .unwrap();
        let config = ReaderConfig::default()
            .notify(false)
            .backoff(blocking(Duration::MAX));
        let mut r = Reader::open(dir.path(), config).unwrap();
        // A timeout past the end of `Instant` means forever; a record is already there.
        assert!(r.next_timeout(Some(Duration::MAX)).unwrap().is_some());
        // Doubling the longest sleep saturates instead of panicking.
        assert!(
            r.next_timeout(Some(Duration::from_millis(30)))
                .unwrap()
                .is_none()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{Reader, SlotOutcome, VALIDATION_RETRIES, VALIDATION_WINDOW, resize_buf};
    use crate::mmap::MAPPED_READS;
    use crate::{CorruptReason, Error, NewRecord, ReaderConfig, Writer, WriterConfig};
    use std::cell::Cell;
    use std::time::Instant;

    thread_local! {
        /// How many of this thread's next non-empty slot reads [`scripted`] turns into a validation failure.
        static FAIL_READS: Cell<u32> = const { Cell::new(0) };
    }

    /// Test builds only: replaces a slot read with `Invalid(BadChecksum)` while [`FAIL_READS`] is non-zero, as if
    /// the bytes had been damaged for that one read.
    pub(super) fn scripted(outcome: SlotOutcome) -> SlotOutcome {
        FAIL_READS.with(|n| match (n.get(), &outcome) {
            (0, _) | (_, SlotOutcome::Empty) => outcome,
            (left, _) => {
                n.set(left - 1);
                SlotOutcome::Invalid(CorruptReason::BadChecksum)
            }
        })
    }

    fn journal_of(count: usize) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            Writer::open(dir.path(), WriterConfig::default().segment_size(65_536)).unwrap();
        for i in 0..count {
            let payload = [i as u8; 100];
            writer
                .append(&NewRecord {
                    schema_id: 0,
                    venue_ts_ns: 0,
                    local_ts_ns: 0,
                    payload: &payload,
                })
                .unwrap();
        }
        dir
    }

    fn open_reader(dir: &std::path::Path, mapped: bool) -> Reader {
        if mapped {
            // SAFETY: only the tickertape writer in `journal_of` modifies the journal, and it has been dropped.
            unsafe { Reader::open_mapped(dir, ReaderConfig::default()) }.unwrap()
        } else {
            Reader::open(dir, ReaderConfig::default()).unwrap()
        }
    }

    /// A slot that fails validation once and validates on the re-read is returned, by both readers: neither reports
    /// corruption for a transient failure. A slot that keeps failing is reported, by both, once it has been read again
    /// for `VALIDATION_WINDOW`.
    #[test]
    fn a_failed_validation_is_retried_by_both_readers() {
        let dir = journal_of(3);
        for mapped in [false, true] {
            let mut reader = open_reader(dir.path(), mapped);
            assert_eq!(reader.try_next().unwrap().unwrap().seq, 0);
            FAIL_READS.with(|n| n.set(1));
            let record = reader.try_next().unwrap().unwrap();
            assert_eq!(
                (record.seq, record.payload),
                (1, &[1u8; 100][..]),
                "mapped: {mapped}"
            );
            FAIL_READS.with(|n| n.set(u32::MAX));
            let start = Instant::now();
            match reader.try_next() {
                Err(Error::Corrupt {
                    reason: CorruptReason::BadChecksum,
                    ..
                }) => {}
                other => panic!("mapped: {mapped}: {other:?}"),
            }
            assert!(start.elapsed() >= VALIDATION_WINDOW, "mapped: {mapped}");
            FAIL_READS.with(|n| n.set(0));
        }
    }

    /// Issue #43: a committed record whose bytes a reader sees wrong for longer than its back-to-back re-reads (on
    /// aarch64 Linux under memory compaction, a mapped view showed a freshly committed record with stale bytes in its
    /// next page for a few microseconds) is returned once the view is right, by both readers, instead of being
    /// reported as corrupt. Before the fix, a fourth failed read was reported as corruption.
    #[test]
    fn a_view_that_catches_up_after_the_back_to_back_re_reads_is_not_reported() {
        let dir = journal_of(3);
        for mapped in [false, true] {
            let mut reader = open_reader(dir.path(), mapped);
            assert_eq!(reader.try_next().unwrap().unwrap().seq, 0);
            // The first read and every back-to-back re-read fail, and a few of the paced ones after them.
            FAIL_READS.with(|n| n.set(VALIDATION_RETRIES + 4));
            let record = reader.try_next().unwrap().unwrap();
            assert_eq!(
                (record.seq, record.payload),
                (1, &[1u8; 100][..]),
                "mapped: {mapped}"
            );
            assert_eq!(FAIL_READS.with(Cell::get), 0, "mapped: {mapped}");
            assert_eq!(reader.try_next().unwrap().unwrap().seq, 2);
        }
    }

    /// `open_mapped` really maps segments where `MAPPED_READS` holds (x86-64 and aarch64), so the differential tests
    /// in `tests/mapped_reader.rs` compare two different readers there; elsewhere it falls back to `pread`. Before and
    /// after a roll, since each segment is opened separately. The safe reader never maps.
    #[test]
    fn open_mapped_maps_segments_where_supported() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            Writer::open(dir.path(), WriterConfig::default().segment_size(65_536)).unwrap();
        let payload = [5u8; 1000];
        let record = NewRecord {
            schema_id: 0,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: &payload,
        };
        for _ in 0..100 {
            writer.append(&record).unwrap();
        }
        // SAFETY: only the tickertape writer above modifies the journal, and it is idle.
        let mut mapped =
            unsafe { Reader::open_mapped(dir.path(), ReaderConfig::default()) }.unwrap();
        let mut safe = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
        assert_eq!(mapped.mapped, MAPPED_READS);
        for n in 0..100 {
            assert_eq!(mapped.seg.map.is_some(), MAPPED_READS, "record {n}");
            assert!(safe.seg.map.is_none());
            assert_eq!(mapped.try_next().unwrap().unwrap().payload, payload);
            assert_eq!(safe.try_next().unwrap().unwrap().payload, payload);
        }
        assert!(
            !mapped.seg.path.ends_with("00000000000000000000.tape"),
            "the records span more than one segment"
        );
        assert_eq!(mapped.seg.map.is_some(), MAPPED_READS);
    }

    /// A frame the address space can't hold is an error, not a `capacity overflow` panic in `Vec::resize`, and the
    /// buffer is left as it was. On 64-bit targets these lengths fit in `usize` but exceed `isize::MAX`; on 32-bit
    /// targets the first already fails the conversion to `usize`.
    #[test]
    fn resize_buf_refuses_unreservable_lengths() {
        for len in [u64::MAX, isize::MAX as u64 + 1] {
            let mut buf = vec![7u8; 16];
            let err = resize_buf(&mut buf, len).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::OutOfMemory, "len {len}");
            assert_eq!(buf, [7u8; 16]);
        }
        let mut buf = vec![7u8; 16];
        resize_buf(&mut buf, 4).unwrap();
        assert_eq!(buf, [7u8; 4]);
        resize_buf(&mut buf, 8).unwrap();
        assert_eq!(buf, [7, 7, 7, 7, 0, 0, 0, 0]);
    }
}
