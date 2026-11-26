//! Readers: replay and live tailing.

use std::fs::File;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{Ordering, fence};
use std::time::{Duration, Instant, SystemTime};

use crate::config::{Backoff, ReaderConfig};
use crate::error::{CorruptReason, Error, Result};
use crate::format::{self, CheckpointSlot, RecordHeader};
use crate::journal::{self, Identity};
use crate::mmap::MapRo;
use crate::record::{OwnedRecord, Record};
use crate::seek_index::{self, codec::Entry, dir::IndexDir, lookup};

/// How long a reader waits at an empty slot before checking that its segment wasn't replaced by recovery.
const IDENTITY_CHECK_INTERVAL: Duration = Duration::from_millis(100);
/// How long a reader waiting at an empty slot goes without looking for a later segment that shows that the journal
/// continues after that slot ([`Reader::check_head`]). It then lists the directory only if that has changed.
const SEGMENT_LIST_INTERVAL: Duration = Duration::from_secs(1);
/// How long a directory must have gone unmodified before it is listed for its modification time to tell a later change:
/// well above the granularity of file systems' clocks.
const DIRECTORY_SETTLED: Duration = Duration::from_secs(2);
/// Bytes the `pread` reader fetches at a time (more if a record is larger), except at the tail ([`Reader::fill_size`]).
const CHUNK: usize = 64 * 1024;
/// Bytes the `pread` reader fetches at a time (more if a record is larger) while it follows the writer with time to
/// spare: the bytes after the newest record are zeros, which it would otherwise copy for nothing, once for every record.
const TAIL_CHUNK: usize = 4 * 1024;
/// How many fills in a row must show that the reader has time to spare before it reads [`TAIL_CHUNK`] ([`Reader::fill_size`]).
const TAIL_FILLS: u16 = 16;
/// While the reads are tail reads, one fill in this many reads a whole chunk anyway ([`Reader::fill_size`]).
const TAIL_PROBE: u16 = 256;
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
#[repr(C, align(16))]
pub struct Reader {
    // The layout is fixed (`#[repr(C, align(16))]`, ADR 0057, issue #86), and `tests::reader_layout` pins the offsets of
    // the fields that the per-record path of either reader reads or writes (marked "Hot"). The order is the one rustc
    // chose before the layout was fixed (`42b2307e`), so that the path compiles as it did then. The record path stores
    // and loads `found` 16 bytes at a time: at a multiple of 16 in a 16-byte aligned struct, none of these accesses can
    // straddle a page boundary, wherever the reader is; one that did made replay 15 to 30 % slower on Linux x86-64 and
    // aarch64. Add new fields at the end (after `used_index`), where they move nothing, and put large state behind a
    // pointer. Moving a hot field needs the replay benchmark on x86-64 and aarch64: grouping the hot fields at the start
    // made the zero-copy reader 2 to 3 % slower on x86-64, for a reason not established.
    dir: PathBuf,
    /// Hot (ADR 0057): the record path uses `size` (at +32) and `map` (at +56).
    seg: OpenSegment,
    /// `pread` buffer: bytes of `seg` starting at file offset `buf_start`. A zero-copy reader uses it only for a record
    /// whose frame crosses `bound` ([`Reader::read_crossing`]), while that record is the current one.
    /// Hot (ADR 0057).
    buf: Vec<u8>,
    /// The journal's wake-up region, if wake-ups are enabled (ADRs 0017 and 0021).
    waker: Option<crate::notify::Waker>,
    /// Hot (ADR 0057): the record path reads `durable_only`.
    config: ReaderConfig,
    /// When a durable-only reader waiting at the checkpoint last checked a segment that the marker has moved past.
    superseded_checked: Option<Instant>,
    /// When the reader started waiting at an empty slot.
    /// Hot (ADR 0057).
    waiting_since: Option<Instant>,
    /// What the reader keeps about its waits at empty slots, for [`Reader::check_head`] and [`Reader::fill_size`].
    /// Boxed (allocated at the open, never again). Before the hot part had a fixed layout, this state as a field moved the
    /// reader so that the zero-copy reader replayed 64-byte records at half the speed on an Apple M4 Pro (ADR 0057).
    wait: Box<Waits>,
    /// Offset of the next slot in `seg`.
    /// Hot (ADR 0057).
    offset: u64,
    /// Sequence number expected at `offset`.
    /// Hot (ADR 0057).
    next_seq: u64,
    /// Exclusive checkpoint boundary, refreshed only when durable-only reads reach it.
    /// Hot (ADR 0057).
    durable_next_seq: u64,
    /// The segment the marker that set `durable_next_seq` names (0 if there was no valid marker).
    durable_segment_base: u64,
    /// The offset in it that the marker names (0 if there was no valid marker).
    durable_offset: u64,
    /// The bound on the zero-copy reader's loads (ADR 0048): the size that `seg` has for them. No frame that would end
    /// after `bound - SEGMENT_TRAILER_LEN` is loaded through the mapping; that is the checkpoint position in `seg` (the
    /// ADR's `limit`). It is the size of `seg`, that is no bound of its own, if the marker bounds nothing at or after the
    /// cursor, if it names a later segment (whose `limit` is `seg`'s size), or once the cursor has reached `limit` and the
    /// marker was read again there (a durable-only reader then has no mapping of `seg`). Checking a frame against this instead of `seg`'s size costs the per-record
    /// path no comparison. Set only by [`Reader::set_limit`], and where a failed `seek_to_end` is undone. The `pread`
    /// reader does not use it.
    /// Hot (ADR 0057).
    bound: u64,
    /// Records before this are skipped (set by `seek`).
    /// Hot (ADR 0057).
    skip_until: u64,
    /// The `next_seq` of a checkpoint marker read before the records the cursor has passed since the last seek that
    /// started at a segment's first record (or at the marker), from files confirmed current after it was read: while
    /// the cursor is at or below it, the cursor is a record boundary of the current chain (ADR 0046). Set only by
    /// seeks, which drop the read buffer; 0 if none, and after a landing through a seek index (ADR 0045).
    seek_covered: u64,
    /// Hot (ADR 0057).
    buf_start: u64,
    /// The record [`Reader::advance`] found last (valid only after it returned `true`). The slot reads decode a
    /// record's header straight into it, so that the per-record path passes only small values around (see
    /// [`SlotOutcome`]).
    /// Hot (ADR 0057).
    found: Found,
    mapped: bool,
    /// Whether the last `seek` landed through a seek index ([`Reader::seek_used_index`]).
    used_index: bool,
}

/// `#[repr(C)]`, in the order rustc chose before it was fixed: `size` at +32 (the `pread` reader) and `map` at +56 (the
/// zero-copy reader) are hot fields of [`Reader`] (ADR 0057).
#[derive(Debug)]
#[repr(C)]
struct OpenSegment {
    path: PathBuf,
    base: u64,
    size: u64,
    identity: Identity,
    /// `None` for the `pread` reader, and for a durable-only zero-copy reader that gave it up
    /// ([`Reader::give_up_mapping`]).
    map: Option<MapRo>,
    file: File,
}

/// A record found by [`Reader::advance`]: its header and where its payload is. `#[repr(C)]`, a hot field of [`Reader`]
/// (ADR 0057).
#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct Found {
    header: RecordHeader,
    /// Offset of the payload in the segment file, plus [`COPIED`] if it is to be handed out from the zero-copy reader's
    /// buffer ([`SlotOutcome::Copied`]).
    payload_at: u64,
    payload_len: usize,
}

/// Marks `Found::payload_at` of a record that the zero-copy reader has in its buffer, not in its mapping: an offset that no
/// mapping reaches, so that the bounds check that every payload in the mapping gets is the only check that record path
/// makes for it.
const COPIED: u64 = 1 << 63;

/// What [`Reader::history_after`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    /// The journal continues after the slot (if that is still empty when read again).
    Shown(u64),
    /// Nothing shows that. If the segments were listed, nothing will while there are the same segments.
    Nothing,
    /// The segments could not be listed, or there is a successor that may come to hold a record.
    Unknown,
}

/// What a reader keeps about its waits at empty slots ([`Reader::wait`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Waits {
    /// The last time [`Reader::check_head`] looked for later segments: `None` if it has not run since the reader was
    /// opened or moved by a seek, or if it ended in an error. It then runs at the next empty slot without waiting first.
    listed: Option<Listed>,
    /// How many fills in a row found the reader following the writer with time to spare, counted from 1 up to
    /// `TAIL_FILLS + TAIL_PROBE - 1` and then from `TAIL_FILLS` again ([`Reader::fill_size`], ADR 0052). Written only by
    /// fills of the `pread` buffer, and by a `seek_to_end` that fails.
    tail_fills: u16,
    /// The wait at the slot the reader last found empty, for [`Reader::fill_size`]. Written only at empty slots
    /// ([`Reader::at_empty_slot`]), never on the record path.
    tail_wait: Option<SlotWait>,
}

/// A wait at an empty slot, as [`Reader::fill_size`] needs it (ADR 0052).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SlotWait {
    /// The segment and offset of the slot.
    base: u64,
    offset: u64,
    /// Whether the reader found the slot empty with the wake-up region attached.
    attached: bool,
    /// Whether it later found the slot empty while the region was taken out, that is, from inside [`Reader::block_on`],
    /// registered as a waiter (the writer then makes a wake-up call for the record). [`Reader::blocked_for`] also counts
    /// a fill made from inside `block_on`, when its check finds the record.
    blocked: bool,
}

/// When a reader at an empty slot looked for later segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Listed {
    at: Instant,
    /// The directory's inode and modification time, if the listing found nothing that could count as evidence later and
    /// another modification would show: while they are the same, there are the same segments. (The cursor only moves
    /// forward until a seek, which forgets this.)
    unchanged: Option<(u64, SystemTime)>,
}

/// Where [`Reader::seek_to_end`] started, to go back to if it fails.
struct Place {
    seg: OpenSegment,
    offset: u64,
    next_seq: u64,
    skip_until: u64,
    seek_covered: u64,
    durable_next_seq: u64,
    durable_segment_base: u64,
    durable_offset: u64,
    bound: u64,
    superseded_checked: Option<Instant>,
    buf: Vec<u8>,
    buf_start: u64,
    waiting_since: Option<Instant>,
    wait: Waits,
    found: Found,
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
pub(crate) enum SlotOutcome {
    Empty,
    Record {
        len: u32,
    },
    /// A record that the zero-copy reader read into its buffer, because its frame crosses `Reader::bound` (ADR 0048).
    Copied {
        len: u32,
    },
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
    /// Below the checkpoint position the reader took last, no load reaches past that position. A record whose frame
    /// crosses that position is copied into the reader's buffer instead (ADR 0048), and a durable-only reader then reads
    /// the rest of that segment through its buffer as well: on an intact journal whose marker fits its files there is
    /// none.
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
    /// With [`ReaderConfig::seek_index`] this also covers `seek-index/` and the checkpoint marker: no process other than
    /// a tickertape writer or index builder ([`seek_index::build`](crate::seek_index::build)) may modify them. Such a
    /// reader uses an index only for a segment before the one the marker names, to which no writer stores; if the marker
    /// is damaged later and a recovery changes such a segment in place, the known limitation below applies. With the
    /// option off, nothing changes.
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
    /// - A commit word holds a non-zero value that no writer stored there, and the frame it claims reaches bytes that
    ///   a writer stores: an earlier record's length, enlarged, or a length or a seal marker in the empty slot at the
    ///   head. The damage can appear whether or not a writer runs. Validating that slot loads bytes the writer is
    ///   storing.
    /// - The commit word of a published record not yet covered by a checkpoint is zeroed, and the next writer's
    ///   recovery cannot tell that from a torn append (damage after which nothing validates, such as the newest
    ///   record's commit word zeroed; `docs/crash-contract.md`, C11 (3), has the cases). It then takes the slot for
    ///   the end of the journal and zeroes that record and the published records after it in place, under a live
    ///   zero-copy reader. A payload the caller still holds then changes, and the reader's later loads meet the new
    ///   writer's stores. (When recovery finds evidence of the records, it usually replaces the segment by a new
    ///   file instead, and mappings of the old file do not change. ADR 0035.)
    ///
    /// The bound that ADR 0048 puts on the loads helps replay and catch-up. A caught-up visible reader tailing the head
    /// gets nothing from it. As long as the journal's `checkpoint` file holds the markers the writer wrote and no earlier
    /// one becomes current, neither case concerns a reader with `durable_only(true)`, and the first does not concern a slot below the
    /// checkpoint position the reader last took (at a segment's open, on reaching that position, and while it waits).
    /// Without that, the limitation is as stated. (For a durable-only reader one case is left, and it takes two faults: a
    /// damaged record that ends exactly at the checkpoint position and passes its CRC32C by chance, and a seal marker
    /// damaged into the empty slot there. The reader then loads the 4 bytes that follow that marker.) ADR 0048.
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
        // Before the segment is opened by its path: the segment was then current after the marker was written.
        let marker = (mapped && !config.durable_only)
            .then(|| marker_for_limit(dir))
            .flatten();
        let (base, seg) = open_listed(dir, mapped, 0, |bases| match bases.oldest {
            Some(base) => Ok(base),
            None => Err(Error::NotFound {
                dir: dir.to_path_buf(),
            }),
        })?;
        let waker = config.notify.then(|| crate::notify::Waker::new(dir));
        let mut reader = Reader {
            dir: dir.to_path_buf(),
            config,
            mapped,
            seg,
            offset: format::SEGMENT_HEADER_LEN as u64,
            next_seq: base,
            durable_next_seq: 0,
            durable_segment_base: 0,
            durable_offset: 0,
            bound: 0,
            superseded_checked: None,
            skip_until: base,
            seek_covered: 0,
            used_index: false,
            buf: Vec::new(),
            buf_start: 0,
            waker,
            waiting_since: None,
            wait: Box::default(),
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
        };
        reader.set_limit(limit_of(marker.flatten().as_ref(), &reader.seg));
        Ok(reader)
    }

    /// Positions the reader so that the next record returned is `seq`.
    ///
    /// If `seq` hasn't been written yet, the reader waits for it: [`Reader::try_next`] returns `None` until the
    /// writer gets there, and records before `seq` are skipped. Every record on the way is validated, so the reader
    /// only lands on positions along the journal's current chain, except as below.
    ///
    /// The way starts at the first record of the segment that holds `seq` (or will hold it), so a seek can cost
    /// reading and validating up to one segment. A forward seek within the reader's segment starts at the reader's
    /// cursor instead, if the reader can tell that the cursor is on the current chain: an earlier seek of this reader
    /// read a checkpoint marker that covers the cursor, the segment's path still names the file the reader has open,
    /// and the last record the reader passed is still where it was (ADR 0046). So the first forward seek in a segment
    /// scans it, and the ones after it continue, as far as the checkpoint reached.
    ///
    /// The reader then lands on the same record as by scanning, except after storage damage of two kinds. Records
    /// before the cursor, which the reader validated when it passed them, are not validated again, so damage done to
    /// covered ones since is not noticed (nor is it by a reader that reads on without seeking). And if damage destroyed
    /// the checkpoint marker, or set it back to an older one, and destroyed covered records, recovery removed those in
    /// place, and the new history holds, right before the cursor, bytes that form a record with the sequence number
    /// expected there, the cursor can be off the current chain without the seek noticing. A seek to the last record
    /// the reader passed (returned or skipped), or an earlier one, scans the segment, and so does a new reader.
    ///
    /// With [`ReaderConfig::seek_index`] (off by default), the way may start at a record before the target that a seek
    /// index names, if there is an index and nothing about it is in doubt (ADR 0045). The record landed on and every
    /// record from there are validated as always. **Records before it are not read:** damage there, at which a seek
    /// without the option stops, goes unnoticed, and the landing place is on the current chain except where a stale or
    /// a forged index coincides with suitable bytes in a payload (`docs/crash-contract.md`, C15). A reader that landed
    /// this way is not at a place a later seek starts from. No seek fails because of an index.
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
        let marker = self.marker_before_open();
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
        let continues = self.continues_from_cursor(seq, &seg);
        if !continues {
            self.seg = seg;
            self.offset = format::SEGMENT_HEADER_LEN as u64;
            self.next_seq = base;
            self.set_limit(limit_of(marker.as_ref(), &self.seg));
        }
        self.used_index = self.config.seek_index && self.land_through_index(seq, continues);
        if self.used_index {
            // Nothing is claimed for a cursor that came from an index, so no later seek starts there (ADR 0045, step 9).
            self.seek_covered = 0;
        } else if !continues || seq >= self.seek_covered {
            // A seek that continues to a target the known marker covers needs no newer one.
            let marker = self.covered_now();
            self.seek_covered = marker.map_or(0, |m| m.next_seq);
            if marker.is_some() && self.visible_mapped() {
                self.set_limit(limit_of(marker.as_ref(), &self.seg));
            }
        }
        self.skip_until = seq;
        self.cursor_moved();
        Ok(())
    }

    /// Positions the reader at the end of the journal, without reading the segments before it: the next record
    /// returned is the first one this call did not find committed (for a durable-only reader, the first one it did
    /// not find covered by a checkpoint).
    ///
    /// Under a live writer, "the end" is an observation made during the call: the reader is placed at a slot that
    /// held no committed record when this call read it. Every record committed before the call is behind the reader,
    /// and every record committed after it returns is ahead of it; one committed during the call can be on either
    /// side. No record is skipped or repeated from there on. A visible reader's end can lie past what a power loss
    /// would keep, like every position of a visible reader.
    ///
    /// The call reads at most the newest segment (from the checkpoint marker's position if the marker points into
    /// it, otherwise from its start) and any segments the writer rolls to meanwhile, validating every record it
    /// reads; records before that are not read, so damage in them is not noticed. A durable-only reader starts at
    /// the marker's position. If the writer keeps committing as fast as the reader validates, the call runs until it
    /// catches up. If there is no valid marker when the call reads it, no record is covered, and a durable-only reader
    /// is placed at the first record of the oldest segment.
    ///
    /// If recovery replaces or removes the segment while the call reads it, the call starts over, at most 8 times.
    /// If it fails, the reader is left exactly as it was. Added in 1.1.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if the journal has no segments; [`Error::Corrupt`], [`Error::UnsupportedVersion`] or
    /// [`Error::Io`] as for [`Reader::open`] and [`Reader::try_next`]; [`Error::Discarded`] if the segment was
    /// replaced or removed on each of 8 attempts in a row. Its `seq` is where the call was reading, not the reader's
    /// position, which is [`Reader::position`] as before the call. A durable-only reader also returns [`Error::Io`] if
    /// the marker can't be read.
    ///
    /// ```no_run
    /// # use tickertape::{Reader, ReaderConfig};
    /// # fn main() -> tickertape::Result<()> {
    /// let mut reader = Reader::open("/tmp/journal", ReaderConfig::default())?;
    /// reader.seek_to_end()?;
    /// while let Some(record) = reader.next_timeout(None)? {
    ///     println!("new: {}", record.seq);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn seek_to_end(&mut self) -> Result<()> {
        let mut attempts = 1;
        loop {
            match self.try_seek_to_end() {
                Err(Error::Discarded { .. }) if attempts < LISTING_ATTEMPTS => attempts += 1,
                result => return result,
            }
        }
    }

    /// Returns the next record if one is committed, without waiting for one.
    ///
    /// `Ok(None)` means there is no new record yet. Follows seals into the next segment transparently.
    ///
    /// An empty slot that cannot be the head of the journal is an error, [`Error::Corrupt`] with
    /// [`CorruptReason::EmptyCommitWord`] (from 1.1; before, the reader waited there for ever): the checkpoint marker
    /// covers it, or a later segment exists that a writer creates only after the slot is filled. The reader looks for that
    /// at the first empty slot after it was opened or moved by a seek, in that call, so that a replay up to the first
    /// `Ok(None)` is told; and at later ones after it has waited there for 100 ms (by the segments alone: at most once a
    /// second, and only if the directory changed since a listing showed nothing; a later segment counts only with a valid
    /// header). No reader gets past such a slot: the records between it and the end of its segment can't be found, and
    /// every call returns the error again. `inspect::verify` reports it too; records after the checkpoint marker can be
    /// repaired by the writer's recovery, covered ones only from a copy of the segment. A slot that was emptied where
    /// nothing above covers it (in the newest segment at or after the marker, for example) can't be told from the head.
    /// `docs/crash-contract.md`, C11.
    ///
    /// One case does wait: a committed slot that fails validation is read again for at least 50 ms before it is
    /// reported, because a reader's view of a record just committed can lag (`docs/crash-contract.md` → Known risks
    /// and limits), and so is an empty slot that is about to be reported. A call that ends in [`Error::Corrupt`], or in [`Error::Discarded`] after a repair, can therefore
    /// take that long, and [`Reader::next_timeout`] can pass its timeout by the same amount.
    /// With [`ReaderConfig::durable_only`] enabled, records wait at the cursor until covered by a checkpoint;
    /// a missing or invalid checkpoint marker covers no records. A marker describes the journal's current files, so a
    /// durable-only reader applies a marker that moves past its cursor only after confirming that its segment is still
    /// in place; if recovery replaced it or retention removed it, the reader returns [`Error::Discarded`] then, instead
    /// of reading on in the file it has open (ADR 0034). If it is in place, the reader reads the records the marker
    /// newly covers from the file's current bytes, so records that recovery removed in place are never returned
    /// (crash contract C13 states the exception: damage that also destroys the checkpoint marker).
    /// A visible reader can't notice such an in-place repair (storage damage that emptied a commit word; crash
    /// contract C11): it may return records it had already read, then continue in the new history, wait, return
    /// [`Error::Discarded`] once the checkpoint or a later segment shows that its position is not the head, or report
    /// corruption at a position that no longer starts a record; [`Reader::seek`] to its position puts it back on the
    /// current chain. (The exceptions of [`Reader::seek`] apply. A reader with [`ReaderConfig::seek_index`] can also have
    /// entered the segment behind the damage, before the repair, where another reader's scan would have stopped; it is
    /// then where a reader that had already passed the damaged record would be. Crash contract C15.)
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`] if the bytes at the cursor fail validation, or if the slot there is empty and can't be the head of
    /// the journal (above). The cursor doesn't move, so calling again retries the same position (which may succeed after a writer recovery).
    /// [`Error::Discarded`] if writer recovery or retention replaced or removed the segment at the cursor
    /// (including when retention removed both it and the next segment while the reader was moving between them), or if
    /// the slot at the cursor is empty, can't be the head, and the record the reader passed last is no longer before it;
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
        // At the checkpoint boundary only a checkpoint helps a durable-only reader (ADR 0053). It waits on the marker
        // words if the writer bumps them. With a writer that does not (1.0.0), it sleeps instead of registering, so that
        // this writer's appends make no wake-up call for it; `next_timeout` looked just before this call, and looks again
        // after it. With a `max_sleep` of zero that would be a busy loop: it then registers on the record words, as 1.0.0
        // does. The capability word is read only for a durable-only reader at the boundary.
        let on = if !self.waits_for_marker() {
            crate::mmap::WaitOn::Record
        } else if notify.writer_bumps_marker() {
            crate::mmap::WaitOn::Marker
        } else if !self.config.backoff.max_sleep.is_zero() {
            let nap = deadline.map_or(self.config.backoff.max_sleep, |d| {
                self.config
                    .backoff
                    .max_sleep
                    .min(d.saturating_duration_since(Instant::now()))
            });
            if !nap.is_zero() {
                std::thread::sleep(nap);
            }
            return (Ok(false), true);
        } else {
            crate::mmap::WaitOn::Record
        };
        #[cfg(test)]
        if let Some(hook) = wake_tests::BEFORE_REGISTER.with(std::cell::Cell::take) {
            hook();
        }
        let seen = notify.begin_wait(on);
        let found = self.advance();
        if !matches!(found, Ok(false))
            || (on == crate::mmap::WaitOn::Marker && !self.waits_for_marker())
        {
            notify.end_wait(on);
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
        if !wait.is_zero() && notify.wait(on, seen, wait) == crate::mmap::WaitOutcome::Unsupported {
            supported = false;
            std::thread::sleep(wait.min(self.config.backoff.max_sleep));
        }
        notify.end_wait(on);
        (Ok(false), supported)
    }

    /// Whether the cursor is at the checkpoint boundary a durable-only reader knows: then only a checkpoint can give it a
    /// record (ADR 0053).
    fn waits_for_marker(&self) -> bool {
        self.config.durable_only && self.next_seq >= self.durable_next_seq
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
    /// It ends at the first slot with no committed record yet (it doesn't wait for more; call `records()` again later
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

    /// Whether a seek to `seq`, which is in `seg` if it exists, can start at the cursor instead of at `seg`'s first
    /// record: whether `seg` is the file the reader has open, with the cursor before `seq`, and the cursor is a record
    /// boundary of the journal's current chain (ADR 0046):
    ///
    /// - The cursor is at or below [`Reader::seek_covered`]: every record the reader passed to get here was covered
    ///   when it was read, by a marker read before, from a file current after that. No recovery changes such records
    ///   (ADR 0034, F2, F3 and F5), and the file is still current, since its path was just opened.
    /// - The record before the cursor is still right before it, read from the file now. This adds nothing while the
    ///   marker is intact; it catches recovery that removed covered records in place because damage had also destroyed or set back
    ///   the marker (outside ADR 0034's argument).
    ///
    /// Anything that can't be established, an I/O error included, is a no: the seek then starts at the segment's first
    /// record. At that record there is nothing to save, and no record before the cursor.
    fn continues_from_cursor(&self, seq: u64, seg: &OpenSegment) -> bool {
        self.next_seq <= seq
            && self.offset > format::SEGMENT_HEADER_LEN as u64
            && self.next_seq <= self.seek_covered
            && seg.base == self.seg.base
            && seg.identity == self.seg.identity
            && self.last_record_in_place().unwrap_or(false)
    }

    /// The checkpoint marker, whose `next_seq` is the exclusive bound of the checkpoint's coverage, if the reader's
    /// segment is still in place after the marker was read (`None` otherwise, and if either can't be established: later
    /// seeks then scan).
    fn covered_now(&self) -> Option<CheckpointSlot> {
        let marker = crate::inspect::read_checkpoint(&self.dir);
        // The marker may become the bound on the loads (ADR 0048 → Ordering).
        fence(Ordering::Acquire);
        crate::fault::hook(crate::fault::Point::ReaderSeekCoverage);
        match marker {
            Ok(Some(marker)) if self.check_identity().is_ok() => Some(marker),
            _ => None,
        }
    }

    /// Whether the reader bounds its loads by markers it reads for that purpose: a zero-copy reader that is not
    /// durable-only (a durable-only one uses the marker it has applied).
    fn visible_mapped(&self) -> bool {
        self.mapped && !self.config.durable_only
    }

    /// For the bound of a segment about to be opened by its path, which proves that it was current after the marker was
    /// written (ADR 0034): the marker, or `None` if there is no valid one, it can't be read, or the reader has no bound.
    /// A durable-only reader does not read it again: files it opens after it read the marker it has applied are current.
    fn marker_before_open(&self) -> Option<CheckpointSlot> {
        if self.config.durable_only {
            return self.durable_marker();
        }
        self.mapped
            .then(|| marker_for_limit(&self.dir))
            .flatten()
            .flatten()
    }

    /// The position of the marker a durable-only reader has applied, as a slot for [`limit_of`].
    fn durable_marker(&self) -> Option<CheckpointSlot> {
        Some(CheckpointSlot {
            generation: 0,
            next_seq: self.durable_next_seq,
            segment_base_seq: self.durable_segment_base,
            offset: self.durable_offset,
        })
    }

    /// Sets the bound for the segment at the cursor, from a `limit` of [`limit_of`]. A limit at or before the cursor has
    /// been reached, and its marker just read, so it bounds nothing: a visible reader then reads without a bound. A
    /// durable-only reader comes to that only while its gate stops it (the marker does not move past its cursor, so it
    /// reads nothing there); otherwise its place and the marker contradict each other, and it gives up its mapping.
    fn set_limit(&mut self, limit: u64) {
        self.drop_copy();
        let size = self.seg.size;
        self.bound = size;
        if self.seg.map.is_none() {
            return;
        }
        if limit > self.offset {
            self.bound = size.min(limit + format::SEGMENT_TRAILER_LEN as u64);
        } else if self.config.durable_only && self.next_seq < self.durable_next_seq {
            self.give_up_mapping();
        }
    }

    /// A durable-only reader whose cursor is at or past the applied marker's position, while its sequence number is below
    /// the marker's: a frame that the marker does not cover was counted as one record (a damaged length that passed its
    /// CRC32C by chance), or the marker does not fit the files. The gate in [`Reader::advance`], which goes by sequence
    /// numbers, does not stop the reader there, where a writer may be storing. So nothing more of this segment is loaded
    /// through the mapping: it is read with `pread`, as the safe reader reads it. The next segment is mapped again.
    fn give_up_mapping(&mut self) {
        self.seg.map = None;
        self.bound = self.seg.size;
    }

    /// Whether there is a bound and the cursor is at or past it.
    fn at_limit(&self) -> bool {
        self.bound < self.seg.size && self.offset + format::SEGMENT_TRAILER_LEN as u64 >= self.bound
    }

    /// Frees the buffer of a zero-copy reader: it has no use for it but the record copied into it last.
    fn drop_copy(&mut self) {
        if self.seg.map.is_some() {
            self.buf = Vec::new();
        }
    }

    /// The cursor has reached the bound. A durable-only reader gives up its mapping. A visible reader reads the marker again, once (the bound is moved or removed either way), and
    /// then checks that the segment is still in place. If either fails, the bound that stays is the one the cursor has
    /// reached, so there is none, and nothing is reported: a reader overtaken by retention reads its segment to the end (crash contract
    /// C14).
    #[inline(never)]
    fn limit_reached(&mut self) {
        if self.config.durable_only {
            return self.give_up_mapping();
        }
        let mut limit = 0;
        if let Some(marker) = marker_for_limit(&self.dir) {
            crate::fault::hook(crate::fault::Point::ReaderLimitIdentity);
            let in_place = crate::fault::inject(crate::fault::Point::ReaderLimitIdentity).is_ok()
                && self.check_identity().is_ok();
            if in_place {
                limit = limit_of(marker.as_ref(), &self.seg);
            }
        }
        self.set_limit(limit);
    }

    /// Whether the last [`Reader::seek`] landed through a seek index. Hidden: for the workspace's tests and benchmarks.
    #[doc(hidden)]
    pub fn seek_used_index(&self) -> bool {
        self.used_index
    }

    /// Tries to place the cursor at an indexed record at or before `target`, in the reader's segment (ADR 0045, "What
    /// a reader does"; the step numbers are those there). Returns whether it did. Otherwise the cursor is where it was,
    /// whatever the reason: nothing about an index is an error.
    ///
    /// `continues`: the seek may start at the cursor, which is then a place to beat. Nothing the caller of `seek` can
    /// see depends on an index except where the seek lands, and no seek does anything here without the option.
    #[inline(never)]
    fn land_through_index(&mut self, target: u64, continues: bool) -> bool {
        self.indexed_place(target, continues).is_some_and(|entry| {
            self.offset = entry.offset;
            self.next_seq = entry.seq;
            if self.visible_mapped() {
                // Step 2's marker names a later segment, and step 5 found this one in place after it was read (the
                // table in ADR 0048, second row). A durable-only reader keeps the bound of the marker it has applied.
                self.set_limit(self.seg.size);
            }
            true
        })
    }

    /// Steps 1 to 8 of [`Reader::land_through_index`]: the entry to land on, its record read and validated. Leaves the
    /// cursor alone; the read buffer is dropped.
    fn indexed_place(&mut self, target: u64, continues: bool) -> Option<Entry> {
        // Step 1.
        let base = self.seg.base;
        if target <= base || self.seg.size <= format::SEGMENT_SIZE_UNIT {
            return None;
        }
        if continues && !seek_index::far(target, base, self.next_seq, self.offset) {
            return None;
        }
        // Step 2: with a marker that names a later segment, no writer stores to this one and no recovery changes it.
        if self.config.durable_only || self.mapped {
            let marker = crate::inspect::read_checkpoint(&self.dir).ok()??;
            if marker.segment_base_seq <= base {
                return None;
            }
            // Loads up to the segment's size then need this read of the marker (ADR 0048 → Ordering).
            fence(Ordering::Acquire);
        }
        crate::fault::hook(crate::fault::Point::ReaderSeekIndexMarker);
        // Steps 3 and 4.
        let dir = IndexDir::open(&self.dir).ok()??;
        let loaded = lookup::load(&dir, &self.seg.file, base, false).ok()?;
        let index = loaded.index().ok()?;
        crate::fault::hook(crate::fault::Point::ReaderSeekIndexLoaded);
        // Step 5, after the index and the marker were read.
        self.check_identity().ok()?;
        // Steps 6 and 7.
        if !lookup::sealed(&self.seg.file, &index).ok()? {
            return None;
        }
        let entry = index.floor(target)?;
        if continues && entry.seq <= self.next_seq {
            return None;
        }
        // Step 8.
        if !lookup::in_place(&self.seg.file, &entry).ok()? {
            return None;
        }
        crate::fault::hook(crate::fault::Point::ReaderSeekIndexLanding);
        self.buf.clear();
        let valid = self.holds_record(&entry).unwrap_or(false);
        // What was read is not kept: every seek leaves the buffer empty, and a mapped reader has no use for it here.
        self.buf.clear();
        self.drop_copy();
        valid.then_some(entry)
    }

    /// Step 8b: whether the slot holds exactly the record `entry` names, as one fresh slot read sees it. Unlike
    /// [`Reader::read_slot`], whose signature is pinned, it is given the commit word to expect, and allocates and reads
    /// nothing unless the slot holds it: the buffer is never sized by a word that was not compared.
    fn holds_record(&mut self, entry: &Entry) -> std::io::Result<bool> {
        let Entry {
            seq,
            offset,
            len,
            crc,
        } = *entry;
        if !(format::RECORD_HEADER_LEN as u32..=format::MAX_FRAME_LEN).contains(&len)
            || !format::record_fits(self.seg.size, offset, len)
        {
            return Ok(false);
        }
        let body_len = u64::from(len) - format::COMMIT_WORD_LEN as u64;
        if let Some(map) = &self.seg.map {
            if map.commit_word(offset) != Some(len) || map.load_u32(offset + 4) != Some(crc) {
                return Ok(false);
            }
            let count = (format::align8(u64::from(len)) - 8) / 8;
            let Some(words) = map.words(offset + 8, count) else {
                return Ok(false);
            };
            // Copied, each word loaded once, rather than decoded in place as the reads after it are: `decode_record_words`
            // keeps its one caller, so that how the compiler builds the per-record path does not depend on this one.
            resize_buf(&mut self.buf, 4 + count * 8)?;
            self.buf[..4].copy_from_slice(&crc.to_le_bytes());
            for (to, word) in self.buf[4..].chunks_exact_mut(8).zip(words.iter()) {
                to.copy_from_slice(&word.to_le_bytes());
            }
            let body = self.buf.get(..body_len as usize);
            return Ok(body.is_some_and(|b| format::decode_record_body(len, b, seq).is_ok()));
        }
        let mut word = [0; format::COMMIT_WORD_LEN];
        self.seg.file.read_exact_at(&mut word, offset)?;
        if u32::from_le_bytes(word) != len {
            return Ok(false);
        }
        fence(Ordering::Acquire);
        resize_buf(&mut self.buf, body_len)?;
        self.seg
            .file
            .read_exact_at(&mut self.buf, offset + format::COMMIT_WORD_LEN as u64)?;
        Ok(self.buf.starts_with(&crc.to_le_bytes())
            && format::decode_record_body(len, &self.buf, seq).is_ok())
    }

    /// Whether the file holds, right before the cursor, a valid record with the sequence number before the cursor's
    /// (checked where the reader last passed a record).
    fn last_record_in_place(&self) -> std::io::Result<bool> {
        let (Some(at), Some(seq)) = (
            (self.found.payload_at & !COPIED).checked_sub(format::RECORD_HEADER_LEN as u64),
            self.next_seq.checked_sub(1),
        ) else {
            return Ok(false);
        };
        let window = (format::RECORD_HEADER_LEN + self.found.payload_len).min(CHUNK);
        let view = journal::PreadView::new(&self.seg.file, self.seg.size, window);
        let slot = format::read_slot(&view, at, seq);
        match view.take_error() {
            Some(e) => Err(e),
            None => Ok(
                matches!(slot, Ok(format::Slot::Record { next_offset, .. }) if next_offset == self.offset),
            ),
        }
    }

    /// Forgets what belongs to the slot the cursor was at: the read buffer (a seek reads the file's current bytes)
    /// and the timers of a wait there.
    fn cursor_moved(&mut self) {
        self.buf.clear();
        self.drop_copy();
        self.waiting_since = None;
        self.wait.listed = None;
        self.superseded_checked = None;
    }

    /// One attempt of [`Reader::seek_to_end`]; on an error the reader is put back where it was.
    fn try_seek_to_end(&mut self) -> Result<()> {
        // Read before the segment is opened by its path, which proves that the file was current after the marker was
        // written: it then holds every record below the marker's position, which is a record boundary (ADR 0034).
        let marker = match crate::inspect::read_checkpoint(&self.dir) {
            Ok(marker) => marker,
            Err(e) if self.config.durable_only => return Err(e),
            // For a visible reader the marker is only a shortcut.
            Err(_) => None,
        };
        // It also bounds a visible zero-copy reader's loads (ADR 0048 → Ordering).
        fence(Ordering::Acquire);
        let newest = match marker {
            _ if !self.config.durable_only => u64::MAX,
            Some(marker) => marker.next_seq,
            None => 0,
        };
        let (base, seg) = open_listed(&self.dir, self.mapped, newest, |bases| {
            bases.floor.or(bases.oldest).ok_or_else(|| Error::NotFound {
                dir: self.dir.clone(),
            })
        })?;
        let (next_seq, offset) = marker
            .filter(|m| {
                m.segment_base_seq == base && crate::status::valid_offset(m.offset, seg.size)
            })
            .map_or((base, format::SEGMENT_HEADER_LEN as u64), |m| {
                (m.next_seq, m.offset)
            });
        let before = Place {
            seg: std::mem::replace(&mut self.seg, seg),
            offset: std::mem::replace(&mut self.offset, offset),
            next_seq: std::mem::replace(&mut self.next_seq, next_seq),
            skip_until: std::mem::replace(&mut self.skip_until, next_seq),
            seek_covered: std::mem::replace(
                &mut self.seek_covered,
                marker.map_or(0, |m| m.next_seq),
            ),
            // A durable-only reader's end is the marker as it is now: an earlier, higher one may be gone.
            durable_next_seq: std::mem::take(&mut self.durable_next_seq),
            durable_segment_base: std::mem::take(&mut self.durable_segment_base),
            durable_offset: std::mem::take(&mut self.durable_offset),
            bound: self.bound,
            superseded_checked: self.superseded_checked,
            buf: std::mem::take(&mut self.buf),
            buf_start: self.buf_start,
            waiting_since: self.waiting_since,
            wait: *self.wait,
            found: self.found,
        };
        self.cursor_moved();
        // For a durable-only reader, until it applies a marker on the way: none (nothing is read before that).
        let limit = marker.filter(|_| self.visible_mapped());
        self.set_limit(limit_of(limit.as_ref(), &self.seg));
        // Without a marker nothing is covered, so a durable-only reader is there. It doesn't look for a marker again:
        // one that has appeared since would make it read the journal from here.
        let end = if self.config.durable_only && marker.is_none() {
            self.check_identity()
        } else {
            self.read_to_end()
        };
        if end.is_err() {
            self.seg = before.seg;
            self.offset = before.offset;
            self.next_seq = before.next_seq;
            self.skip_until = before.skip_until;
            self.seek_covered = before.seek_covered;
            // A marker applied on the way was checked against the segment read there, not against this one (C13).
            self.durable_next_seq = before.durable_next_seq;
            self.durable_segment_base = before.durable_segment_base;
            self.durable_offset = before.durable_offset;
            self.bound = before.bound;
            self.superseded_checked = before.superseded_checked;
            self.buf = before.buf;
            self.buf_start = before.buf_start;
            self.waiting_since = before.waiting_since;
            *self.wait = before.wait;
            self.found = before.found;
        }
        end
    }

    /// Moves the cursor past every record there is to read, then checks that it did so in a file that is still part
    /// of the journal.
    fn read_to_end(&mut self) -> Result<()> {
        while self.advance()? {}
        self.check_identity()
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
        let slot = self.read_slot_bounded()?;
        // A record found here is not returned.
        self.drop_copy();
        match slot {
            SlotOutcome::Seal => self.follow_seal(),
            SlotOutcome::Empty => {
                self.check_identity()?;
                // The seal of a segment before the marker's is missing. Asked again by the next call if so.
                self.superseded_checked = None;
                self.report_unless_head(false)?;
                self.superseded_checked = Some(now);
                Ok(())
            }
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
        let (durable, segment_base, offset) = crate::inspect::read_checkpoint(&self.dir)?
            .map_or((0, 0, 0), |c| (c.next_seq, c.segment_base_seq, c.offset));
        // The marker also bounds a zero-copy reader's loads (ADR 0048 → Ordering).
        fence(Ordering::Acquire);
        if durable > self.next_seq {
            crate::fault::hook(crate::fault::Point::ReaderCheckpointIdentity);
            self.check_identity()?;
            // Bytes buffered before the marker was read may predate an in-place repair after the old marker.
            self.buf.clear();
        }
        self.durable_next_seq = durable;
        self.durable_segment_base = segment_base;
        self.durable_offset = offset;
        if self.seg.map.is_some() {
            self.set_limit(limit_of(self.durable_marker().as_ref(), &self.seg));
        }
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
                SlotOutcome::Copied { len } => return self.step_past_copied(len),
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
            let read = self.read_slot_bounded()?;
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

    /// [`Reader::step_past_record`] for a record whose payload is in the zero-copy reader's buffer. The cursor is then past
    /// the bound: a durable-only reader gives up its mapping (and hands the payload out as the `pread` reader does).
    #[inline(never)]
    fn step_past_copied(&mut self, len: u32) -> Result<bool> {
        let stepped = self.step_past_record(len);
        if self.config.durable_only {
            self.give_up_mapping();
        } else {
            self.found.payload_at |= COPIED;
        }
        stepped
    }

    #[inline(never)]
    fn sequence_exhausted(&self) -> Error {
        Error::corrupt(
            &self.seg.path,
            self.offset,
            CorruptReason::SequenceExhausted,
        )
    }

    /// At an empty slot: [`Reader::check_head`], once per [`IDENTITY_CHECK_INTERVAL`] of waiting there, and without
    /// waiting at the first empty slot after the reader was opened or moved by a seek: an application that replays until
    /// `Ok(None)` polls it once. Like any slot, it renews a visible zero-copy reader's bound when that has just been
    /// reached.
    #[inline(never)]
    fn at_empty_slot(&mut self) -> Result<()> {
        if self.at_limit() {
            self.limit_reached();
        }
        let now = Instant::now();
        let since = *self.waiting_since.get_or_insert(now);
        let waited = now.duration_since(since) >= IDENTITY_CHECK_INTERVAL;
        self.note_wait();
        if waited || self.wait.listed.is_none() {
            self.check_head(now, waited)?;
        }
        Ok(())
    }

    /// Keeps, for [`Reader::fill_size`], that the reader found the slot at the cursor empty, with or without the wake-up
    /// region attached, and whether it found it empty again while registered as a waiter: [`Reader::block`] takes the
    /// region out of the waker for the whole of [`Reader::block_on`], whose check of the slot comes here. A reader
    /// without a region never counts as blocked; the writer makes no wake-up calls for it.
    fn note_wait(&mut self) {
        let attached = self
            .waker
            .as_ref()
            .is_some_and(crate::notify::Waker::attached);
        let slot = (self.seg.base, self.offset);
        match &mut self.wait.tail_wait {
            Some(wait) if (wait.base, wait.offset) == slot => {
                if attached {
                    wait.attached = true;
                } else if wait.attached {
                    wait.blocked = true;
                }
            }
            other => {
                *other = Some(SlotWait {
                    base: slot.0,
                    offset: slot.1,
                    attached,
                    blocked: false,
                });
            }
        }
    }

    /// Whether the empty slot at the cursor can be the head of the journal (ADR 0050): an error if the checkpoint marker
    /// or a later segment shows that it is not. If the reader has `waited`, also checks that its segment is still in
    /// place, and a visible zero-copy reader takes its bound from the marker read before that check; if the marker
    /// can't be read, the old bound stays.
    #[inline(never)]
    fn check_head(&mut self, now: Instant, waited: bool) -> Result<()> {
        crate::fault::hook(crate::fault::Point::ReaderEmptySlotCheck);
        let bounds = waited && self.visible_mapped();
        let marker = if bounds {
            marker_for_limit(&self.dir)
        } else {
            read_marker(&self.dir)
        };
        if waited {
            self.waiting_since = Some(now);
            self.check_identity()?;
            if let Some(marker) = marker.as_ref().filter(|_| bounds) {
                self.set_limit(limit_of(marker.as_ref(), &self.seg));
            }
        }
        let previous = self.wait.listed;
        let due = previous.is_none_or(|l| now.duration_since(l.at) >= SEGMENT_LIST_INTERVAL);
        // Before the listing.
        let directory = due.then(|| directory_stamp(&self.dir)).flatten();
        let unchanged = previous.and_then(|l| l.unchanged);
        let list = due && (unchanged.is_none() || unchanged != directory);
        let follows = self.history_after(marker.flatten().as_ref(), list);
        let listed = match previous {
            Some(listed) if !list => Listed {
                at: if due { now } else { listed.at },
                ..listed
            },
            _ => Listed {
                at: now,
                unchanged: directory.filter(|d| follows == After::Nothing && settled(d.1)),
            },
        };
        if matches!(follows, After::Shown(_)) {
            // After an error the next call checks again at once, and so reports the slot again.
            self.wait.listed = None;
            self.report_unless_head(list)?;
        }
        self.wait.listed = Some(listed);
        Ok(())
    }

    /// What shows that the journal continues after the empty slot at the cursor, if its segment is still in place and the
    /// slot is still empty when it is read after this (ADR 0050): the marker's `next_seq`, or the base of a later segment.
    ///
    /// - The marker covers the slot, or names a later segment. A writer stores a record, and the seal in front of a
    ///   segment it rolls to, before it writes a marker that covers them, and no recovery from that marker changes them
    ///   (ADR 0034, F2).
    /// - If `list`: a segment with a base above the cursor exists, with a valid header (a writer links a segment's name
    ///   only after it wrote the header, and only after it committed every record below its base); or one with the
    ///   cursor's sequence number, not the reader's own, that holds a record, which a writer appends only after it sealed
    ///   the segment before. (One without a record does not count: a writer links it before it stores the seal, and may
    ///   die in between. A file with such a name and no valid header does not count either: it is no segment of a writer.
    ///   One with a valid header from another journal does, as it does for recovery and `verify`: ADR 0050.)
    ///
    /// What can't be read, the marker or a segment, shows nothing.
    fn history_after(&self, marker: Option<&CheckpointSlot>, list: bool) -> After {
        if let Some(m) = marker {
            if m.next_seq > self.next_seq || m.segment_base_seq > self.seg.base {
                return After::Shown(m.next_seq);
            }
        }
        if !list {
            return After::Nothing;
        }
        crate::fault::hook(crate::fault::Point::ReaderEmptySlotList);
        let Ok((mut later, next)) = journal::bases_from(&self.dir, self.next_seq) else {
            return After::Unknown;
        };
        // The newest first: on a journal that continues, the first one opened decides.
        later.sort_unstable_by(|a, b| b.cmp(a));
        if let Some(&base) = later.iter().find(|&&base| is_segment(&self.dir, base)) {
            return After::Shown(base);
        }
        let at_next = match next {
            false => After::Nothing,
            true if self.next_seq == self.seg.base => After::Nothing,
            true if holds_a_record(&self.dir, self.next_seq) => After::Shown(self.next_seq),
            true => After::Unknown,
        };
        match at_next {
            // Names that are not segments (yet: a header that never reached the disk can be restored in place) leave
            // the question open, but hide no evidence.
            After::Nothing if !later.is_empty() => After::Unknown,
            found => found,
        }
    }

    /// [`Reader::history_after`] found something: reports the empty slot at the cursor if that stands.
    ///
    /// The slot is read again from the file (not the mapping, which can lag), for [`VALIDATION_WINDOW`] like any slot
    /// about to be reported, then once more after the evidence and the segment's identity were checked again: a writer
    /// stores the record or the seal before it produces the evidence, so a slot that is empty when read after it, in a file
    /// that is still in place, was emptied. `Ok` if any of this fails to hold: the reader waits on.
    ///
    /// A visible reader whose records were removed by a repair in place can be at an offset that starts no record of the
    /// new history (crash contract, "Readers after a repair"). What is there is no damage. That cursor is told by the
    /// record before it, which is no longer there: then `Error::Discarded`.
    #[inline(never)]
    fn report_unless_head(&mut self, list: bool) -> Result<()> {
        crate::fault::hook(crate::fault::Point::ReaderEmptySlot);
        let deadline = Instant::now() + VALIDATION_WINDOW;
        let mut pause = VALIDATION_PAUSE_MIN;
        loop {
            if !self.slot_is_empty()? {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            std::thread::sleep(pause.min(deadline - now));
            pause = (pause * 2).min(VALIDATION_PAUSE_MAX);
        }
        let marker = read_marker(&self.dir).flatten();
        let After::Shown(later) = self.history_after(marker.as_ref(), list) else {
            return Ok(());
        };
        crate::fault::hook(crate::fault::Point::ReaderEmptySlotConfirm);
        self.check_identity()?;
        fence(Ordering::Acquire);
        if !self.slot_is_empty()? {
            return Ok(());
        }
        // At a segment's first slot, and at or below what a seek knows to be covered, the cursor is a record boundary.
        if self.offset == format::SEGMENT_HEADER_LEN as u64
            || self.next_seq <= self.seek_covered
            || self
                .last_record_in_place()
                .map_err(|e| Error::io("read segment", &self.seg.path, e))?
        {
            return Err(Error::corrupt(
                &self.seg.path,
                self.offset,
                CorruptReason::EmptyCommitWord(later),
            ));
        }
        Err(Error::Discarded {
            seq: self.position(),
        })
    }

    /// Whether the commit word at the cursor is 0 in the file.
    fn slot_is_empty(&self) -> Result<bool> {
        let mut word = [0; format::COMMIT_WORD_LEN];
        self.seg
            .file
            .read_exact_at(&mut word, self.offset)
            .map_err(|e| Error::io("read segment", &self.seg.path, e))?;
        Ok(word == [0; format::COMMIT_WORD_LEN])
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
        let marker = self.marker_before_open();
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
                let sealed = matches!(self.read_slot_bounded()?, SlotOutcome::Seal);
                // A record found here is read again by the caller.
                self.drop_copy();
                if !sealed {
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
        self.set_limit(limit_of(marker.as_ref(), &self.seg));
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
            // A frame that would end after `bound` fails the bounds checks, so nothing of it is loaded; the retry then
            // looks at why (ADR 0048, `Reader::read_slot_bounded`).
            return Ok(read_mapped(
                map,
                word,
                offset,
                self.bound,
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

    /// A fresh [`Reader::read_slot`] of a slot that is perhaps invalid only because of the bound: where the cursor has
    /// reached the limit, renews the bound and reads the slot again, and where the slot's frame crosses the bound, going by
    /// its commit word, reads it with `pread`. (A slot at or past the limit, or whose frame ends after it, is always
    /// `Invalid` for `read_slot`, except an empty one and a seal at the limit. So it is the retry of an invalid slot that
    /// comes here, and the per-record path pays nothing for it.)
    #[inline(never)]
    fn read_slot_bounded(&mut self) -> Result<SlotOutcome> {
        let mut read = self.read_slot(true)?;
        if self.bound == self.seg.size || !matches!(read, SlotOutcome::Invalid(_)) {
            return Ok(read);
        }
        if self.at_limit() {
            self.limit_reached();
            read = self.read_slot(true)?;
        }
        match read {
            // A length that no frame has, and a seal in front of the limit (which ends at or before it, since the limit is
            // a multiple of 8), are invalid wherever the bound is.
            SlotOutcome::Invalid(CorruptReason::BadLength(word))
                if self.bound < self.seg.size && word >= format::RECORD_HEADER_LEN as u32 =>
            {
                self.read_crossing()
            }
            read => Ok(read),
        }
    }

    /// Reads the slot at the cursor of a mapped segment as the `pread` reader reads a fresh slot, commit word included,
    /// and with the same outcome, into the buffer. Nothing of it but the commit word has been loaded through the mapping
    /// (ADR 0048): the frame crosses the checkpoint position, so its length is damaged, or the marker does not fit the
    /// files. The buffer holds no more than the claimed frame, which lies in the segment, and is kept only for a valid
    /// record, until the next slot is read.
    fn read_crossing(&mut self) -> Result<SlotOutcome> {
        let outcome = self.read_into_buf();
        if !matches!(outcome, Ok(SlotOutcome::Copied { .. })) {
            self.drop_copy();
        }
        outcome
    }

    fn read_into_buf(&mut self) -> Result<SlotOutcome> {
        let io = |e| Error::io("read segment", &self.seg.path, e);
        let offset = self.offset;
        let mut word = [0u8; format::COMMIT_WORD_LEN];
        self.seg.file.read_exact_at(&mut word, offset).map_err(io)?;
        let word = u32::from_le_bytes(word);
        if word == 0 {
            return Ok(SlotOutcome::Empty);
        }
        fence(Ordering::Acquire);
        let pending = classify(word, offset, self.seg.size);
        let body_len = pending_len(pending);
        resize_buf(&mut self.buf, format::COMMIT_WORD_LEN as u64 + body_len)
            .map_err(|e| Error::io("allocate read buffer", &self.seg.path, e))?;
        self.buf_start = offset;
        let body = &mut self.buf[format::COMMIT_WORD_LEN..];
        self.seg
            .file
            .read_exact_at(body, offset + format::COMMIT_WORD_LEN as u64)
            .map_err(io)?;
        let outcome = finish(
            pending,
            word,
            self.next_seq,
            Some(body),
            &mut self.found.header,
        );
        Ok(match outcome {
            SlotOutcome::Record { len } => SlotOutcome::Copied { len },
            other => other,
        })
    }

    /// How many bytes the fill at `offset` asks for: [`CHUNK`], unless the reader follows the writer with time to spare,
    /// when it is [`TAIL_CHUNK`]. Not observable except through timing and system calls (ADR 0052).
    ///
    /// A fill *follows* if the slot at the cursor was found empty before it (so the reader was waiting for this record),
    /// the reader used exactly one record of the last fill (so it was waiting for that one too), and the reader did not
    /// block for this record ([`Reader::blocked_for`]). A blocking reader that reads less blocks for more records, and the
    /// writer pays a wake-up call for each, so a reader that blocks reads whole chunks, however long it waited; one that
    /// stops blocking counts again from its first fill that follows. After [`TAIL_FILLS`]
    /// such fills in a row, that one included, the reads are tail reads, for as long as every fill follows, except that one
    /// in [`TAIL_PROBE`] of them reads a whole chunk anyway. The first one that does not follow is a whole chunk again, and
    /// the count starts over: the reader found the next record without waiting, or more than one record in the last fill.
    /// So the reader takes tail reads only where it has shown that it waits for the writer even when it reads whole
    /// chunks, and keeps showing it: a reader that waits for every record only because its tail reads are fast would
    /// not, after a whole chunk, wait for the next one, and goes back to whole chunks. A fill always reads a whole chunk
    /// if it is
    ///
    /// - for a record the reader blocked for;
    ///
    /// - on replay or catch-up, where no slot is empty before its fill;
    /// - the first one after a seek and in a segment, and one of a durable-only reader, which comes to no empty slot
    ///   on an intact journal (a seek, a seal and each record passed end the wait at a slot);
    /// - a repeated one of the same slot, when its record failed validation.
    fn fill_size(&mut self, offset: u64) -> u64 {
        // `waiting_since` is set at an empty slot and cleared when the cursor moves. The last fill began at a slot, so the
        // buffer starts with that slot's commit word, unless it was emptied since (by a seek, a seal, a marker refresh or a
        // failed read); the first slot of a segment lies at or before the start of the last fill, in the segment before.
        let used = offset.saturating_sub(self.buf_start);
        let first = self.buf.first_chunk::<4>().map(|w| u32::from_le_bytes(*w));
        // (A seal is never at the start of the buffer: following one empties it. The check is a guard.)
        let follows = self.waiting_since.is_some()
            && first
                .is_some_and(|w| w != format::SEAL_MARKER && format::align8(u64::from(w)) == used)
            && !self.blocked_for(offset);
        // The fill counted `probe` reads a whole chunk; the count then goes on as from the first tail read.
        let probe = TAIL_FILLS + TAIL_PROBE - 1;
        self.wait.tail_fills = match self.wait.tail_fills {
            _ if !follows => 0,
            n if n == probe => TAIL_FILLS,
            n => n + 1,
        };
        if (TAIL_FILLS..probe).contains(&self.wait.tail_fills) {
            TAIL_CHUNK as u64
        } else {
            CHUNK as u64
        }
    }

    /// Whether the reader registered as a waiter for the record at `offset`: it found the slot empty again from inside
    /// [`Reader::block_on`], or it is filling from inside `block_on` now (its check found the record), which shows as the
    /// wake-up region taken out of the waker after it was attached when the slot was first found empty.
    fn blocked_for(&self, offset: u64) -> bool {
        let taken = || {
            self.waker
                .as_ref()
                .is_some_and(|w| !crate::notify::Waker::attached(w))
        };
        self.wait.tail_wait.is_some_and(|w| {
            (w.base, w.offset) == (self.seg.base, offset) && (w.blocked || (w.attached && taken()))
        })
    }

    /// Fills the `pread` buffer from `offset`: at least the frame of commit word `word`, normally as much as
    /// [`Reader::fill_size`] says, but never past the end of the segment.
    fn fill_buf(&mut self, offset: u64, word: u32) -> Result<()> {
        let want = if word == format::SEAL_MARKER {
            format::SEAL_LEN as u64
        } else {
            u64::from(word)
        };
        let len = want
            .max(self.fill_size(offset))
            .min(self.seg.size.saturating_sub(offset));
        #[cfg(test)]
        tests::note_fill(len);
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

    /// The payload of a record that the zero-copy reader found last and has in its buffer (from the commit word on).
    #[inline(never)]
    fn copied_payload(&self) -> &[u8] {
        (self.found.payload_at & !COPIED)
            .checked_sub(self.buf_start)
            .and_then(|start| usize::try_from(start).ok())
            .and_then(|start| {
                self.buf
                    .get(start..start.checked_add(self.found.payload_len)?)
            })
            .unwrap_or(&[])
    }

    /// The record [`Reader::advance`] just found, borrowing its payload from the mapping or the buffer.
    fn materialize(&self) -> Record<'_> {
        let found = &self.found;
        let payload: &[u8] = match &self.seg.map {
            Some(map) => {
                // SAFETY: the payload is part of a record validated through this mapping after observing its non-zero
                // commit word; on an intact publication chain `open_mapped`'s contract makes it immutable while the
                // reader exists (ADR 0027 → "Which loads can race" describes both cases after storage damage, which
                // ADR 0048 narrows). The only records not validated through the mapping are those marked `COPIED`,
                // whose offset is out of the mapping's bounds: for them this is `None`.
                let mapped =
                    unsafe { map.committed_bytes(found.payload_at, found.payload_len as u64) };
                match mapped {
                    Some(payload) => payload,
                    None => self.copied_payload(),
                }
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

/// The inode and modification time of the journal's directory (which linking, renaming or removing a segment changes).
fn directory_stamp(dir: &Path) -> Option<(u64, SystemTime)> {
    let meta = std::fs::metadata(dir).ok()?;
    Some((meta.ino(), meta.modified().ok()?))
}

/// Whether a directory modified at `modified` would get another modification time if it were modified now.
fn settled(modified: SystemTime) -> bool {
    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age >= DIRECTORY_SETTLED)
}

/// The checkpoint marker, as [`marker_for_limit`] reads it.
fn read_marker(dir: &Path) -> Option<Option<CheckpointSlot>> {
    let marker = crate::inspect::read_checkpoint(dir).ok()?;
    fence(Ordering::Acquire);
    Some(marker)
}

/// Whether the file named for the segment with base `base` has a valid segment header (for that base and its own size).
fn is_segment(dir: &Path, base: u64) -> bool {
    let path = journal::segment_path(dir, base);
    let valid =
        File::open(&path).is_ok_and(|file| journal::read_header(&file, &path, base).is_ok());
    fence(Ordering::Acquire);
    valid
}

/// Whether the segment with base `base` has a valid record in its first slot. Anything that can't be read is a no.
fn holds_a_record(dir: &Path, base: u64) -> bool {
    let path = journal::segment_path(dir, base);
    let Ok(file) = File::open(&path) else {
        return false;
    };
    let Ok(header) = journal::read_header(&file, &path, base) else {
        return false;
    };
    let view = journal::PreadView::new(&file, header.segment_size, CHUNK);
    let holds = matches!(
        format::read_slot(&view, format::SEGMENT_HEADER_LEN as u64, base),
        Ok(format::Slot::Record { .. })
    );
    fence(Ordering::Acquire);
    holds
}

/// The checkpoint marker for the bound of a visible zero-copy reader (ADR 0048): `Some(None)` if there is no valid one,
/// `None` if it can't be read, which is never reported.
///
/// The marker's writer stored everything before the marker's position before it wrote the marker. The `pread` returned
/// the marker, and the fence orders the mapped loads that follow after it, so those below the position meet no store,
/// also where the reader has not acquired the commit word of the record they touch (ADR 0048 → Ordering).
#[inline(never)]
fn marker_for_limit(dir: &Path) -> Option<Option<CheckpointSlot>> {
    crate::fault::hook(crate::fault::Point::ReaderLimitMarker);
    crate::fault::inject(crate::fault::Point::ReaderLimitMarker).ok()?;
    read_marker(dir)
}

/// How far `marker` covers `seg` (the table in ADR 0048): the marker's offset if it names `seg`, `seg`'s size if it names
/// a later segment (then no writer stores to `seg` and no recovery from this marker changes it), and otherwise 0.
fn limit_of(marker: Option<&CheckpointSlot>, seg: &OpenSegment) -> u64 {
    match marker {
        Some(m) if m.segment_base_seq > seg.base => seg.size,
        Some(m)
            if m.segment_base_seq == seg.base
                && crate::status::valid_offset(m.offset, seg.size) =>
        {
            m.offset
        }
        _ => 0,
    }
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
/// the same bytes. A record's header goes to `header`. `size` is the segment's, or less (`Reader::bound`): no byte of a
/// frame that does not fit in it is loaded, and the frame is `Invalid`.
pub(crate) fn read_mapped(
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
#[path = "reader_bound_tests.rs"]
mod bound_tests;

#[cfg(test)]
mod wake_tests {
    use super::*;

    thread_local! {
        /// Runs once in `block_on`, after the reader chose which words to wait on and before it registers.
        pub(super) static BEFORE_REGISTER: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::Cell::new(None) };
    }
    use crate::mmap::test_hooks::{BEGINS, FORCE_UNSUPPORTED, LOCK, NO_CAPABILITIES, WAITS};
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

    /// Resets `NO_CAPABILITIES` even if the test panics.
    struct Capable;
    impl Drop for Capable {
        fn drop(&mut self) {
            NO_CAPABILITIES.store(false, Ordering::SeqCst);
        }
    }

    /// A writer that appends 30 records 10 ms apart, none of them checkpointed, then checkpoints, while a durable-only
    /// reader that has read everything checkpointed before blocks. Returns the reader's registrations as a waiter on
    /// the record words and on the marker words, and how long after the checkpoint it got the next record.
    fn durable_reader_under_appends(sleep: Duration) -> (u64, u64, Duration) {
        let dir = tempfile::tempdir().unwrap();
        let mut w =
            Writer::open(dir.path(), WriterConfig::default().segment_size(1 << 20)).unwrap();
        let record = |w: &mut Writer| {
            w.append(&crate::NewRecord {
                schema_id: 1,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: b"x",
            })
            .unwrap()
        };
        for _ in 0..5 {
            record(&mut w);
        }
        w.checkpoint().unwrap();
        let mut r = Reader::open(
            dir.path(),
            ReaderConfig::default()
                .durable_only(true)
                .backoff(blocking(sleep)),
        )
        .unwrap();
        for seq in 0..5 {
            assert_eq!(r.try_next().unwrap().unwrap().seq, seq);
        }
        let before = [0, 1].map(|i| BEGINS[i].load(Ordering::SeqCst));
        let writer = std::thread::spawn(move || {
            for _ in 0..30 {
                record(&mut w);
                std::thread::sleep(Duration::from_millis(10));
            }
            w.checkpoint().unwrap();
            (w, Instant::now())
        });
        let got = r.next_timeout(Some(Duration::from_secs(10))).unwrap();
        let received = Instant::now();
        assert_eq!(got.map(|record| record.seq), Some(5));
        let (_w, checkpointed) = writer.join().unwrap();
        let begins = [0, 1].map(|i| BEGINS[i].load(Ordering::SeqCst) - before[i]);
        (
            begins[0],
            begins[1],
            received.saturating_duration_since(checkpointed),
        )
    }

    /// ADR 0053: at the checkpoint boundary a durable-only reader waits on the marker words, so the appends never find
    /// it registered on theirs, and the checkpoint wakes it.
    #[test]
    fn a_durable_only_reader_at_the_boundary_waits_on_the_marker_words() {
        let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (record, marker, after) = durable_reader_under_appends(Duration::from_secs(10));
        assert_eq!(record, 0, "registered on the record words");
        assert!(marker >= 1, "never registered on the marker words");
        // A lost wake-up costs the 100 ms cap; the poll interval is 10 s.
        assert!(
            after < Duration::from_millis(90),
            "woken {after:?} after the checkpoint"
        );
    }

    /// ADR 0053: with a writer that does not bump the marker words (capabilities 0, as 1.0.0), a durable-only reader at
    /// the boundary sleeps `max_sleep` at a time without registering, so that writer makes no wake-up call for it.
    #[test]
    fn a_durable_only_reader_sleeps_unregistered_beside_a_writer_without_marker_words() {
        let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _capable = Capable;
        NO_CAPABILITIES.store(true, Ordering::SeqCst);
        let (record, marker, after) = durable_reader_under_appends(Duration::from_millis(5));
        assert_eq!((record, marker), (0, 0), "registered as a waiter");
        assert!(
            after < Duration::from_millis(90),
            "{after:?} after the checkpoint"
        );
    }

    /// A durable-only reader at the boundary of 5 checkpointed records, which it has read, and the region it would block
    /// on (taken out of its waker, so that a test can call `block_on` with a deadline of its own).
    fn reader_at_the_boundary(dir: &std::path::Path) -> (Writer, Reader, crate::mmap::Notify) {
        let mut w = Writer::open(dir, WriterConfig::default().segment_size(1 << 20)).unwrap();
        for _ in 0..5 {
            append_one(&mut w);
        }
        w.checkpoint().unwrap();
        let mut r = Reader::open(
            dir,
            ReaderConfig::default()
                .durable_only(true)
                .backoff(blocking(Duration::from_secs(10))),
        )
        .unwrap();
        for seq in 0..5 {
            assert_eq!(r.try_next().unwrap().unwrap().seq, seq);
        }
        let (_, notify) = r.waker.as_mut().unwrap().take().unwrap();
        (w, r, notify)
    }

    fn append_one(w: &mut Writer) -> u64 {
        w.append(&crate::NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: &[7; 8],
        })
        .unwrap()
    }

    /// A checkpoint between the reader's choice of the marker words and its registration bumps the count before the
    /// reader reads it, so no wake-up will come: its look after registering must find the record. Without that look it
    /// sleeps out the 100 ms cap (the deadline here is 10 s).
    #[test]
    fn a_checkpoint_between_the_choice_and_the_registration_is_not_lost() {
        let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let (w, mut r, notify) = reader_at_the_boundary(dir.path());
        let w = std::rc::Rc::new(std::cell::RefCell::new(w));
        let hooked = std::rc::Rc::new(std::cell::Cell::new(None));
        let (writer, at) = (w.clone(), hooked.clone());
        BEFORE_REGISTER.set(Some(Box::new(move || {
            let mut w = writer.borrow_mut();
            append_one(&mut w);
            w.checkpoint().unwrap();
            at.set(Some(Instant::now()));
        })));
        let (found, _) = r.block_on(&notify, Some(Instant::now() + Duration::from_secs(10)));
        let after = hooked.get().expect("the hook did not run").elapsed();
        assert!(found.unwrap(), "no record after the checkpoint");
        assert!(
            after < Duration::from_millis(50),
            "returned {after:?} after the checkpoint"
        );
        assert_eq!(r.found.header.seq, 5);
    }

    /// The reader chose the marker words at its old boundary; its look then applies a new marker that covers a slot whose
    /// commit word storage damage emptied. A reader that has already waited at an empty slot checks such a slot against
    /// the marker only after 100 ms there (ADR 0050), so the look returns nothing although it moved the boundary. It must
    /// return to choose again (the record words, which appends bump), not sleep on the marker words, which nothing bumps
    /// here until the 100 ms cap.
    #[test]
    fn a_look_that_moves_the_boundary_without_a_record_chooses_again() {
        use std::os::unix::fs::FileExt;
        let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let (w, mut r, notify) = reader_at_the_boundary(dir.path());
        // It has waited at the empty slot after the checkpointed records once already, and (whatever the checkpoint in
        // the hook takes on a loaded machine) for less than the 100 ms after which it would check the slot against the
        // marker: a start of waiting in the future reads as no time waited.
        r.at_empty_slot().unwrap();
        r.waiting_since = Some(Instant::now() + Duration::from_secs(3600));
        let w = std::rc::Rc::new(std::cell::RefCell::new(w));
        let hooked = std::rc::Rc::new(std::cell::Cell::new(None));
        let (writer, at, path) = (w.clone(), hooked.clone(), dir.path().to_path_buf());
        BEFORE_REGISTER.set(Some(Box::new(move || {
            let mut w = writer.borrow_mut();
            assert_eq!(append_one(&mut w), 5);
            let frame = format::align8(u64::from(format::frame_len(8).unwrap()));
            std::fs::OpenOptions::new()
                .write(true)
                .open(path.join(format::segment_file_name(0)))
                .unwrap()
                .write_all_at(&[0; 4], format::SEGMENT_HEADER_LEN as u64 + 5 * frame)
                .unwrap();
            w.checkpoint().unwrap();
            at.set(Some(Instant::now()));
        })));
        let (found, _) = r.block_on(&notify, Some(Instant::now() + Duration::from_secs(10)));
        let after = hooked.get().expect("the hook did not run").elapsed();
        assert!(!found.unwrap(), "a record in an emptied slot");
        assert!(!r.waits_for_marker(), "the look did not move the boundary");
        assert!(
            after < Duration::from_millis(50),
            "returned {after:?} after the checkpoint: it slept on the marker words"
        );
    }

    /// With `max_sleep` zero, a durable-only reader beside a writer without the marker words must not loop without
    /// sleeping: it registers on the record words, as 1.0.0 does, and blocks up to the 100 ms cap per wait.
    #[test]
    fn a_reader_with_no_sleep_beside_a_writer_without_marker_words_does_not_spin() {
        let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _capable = Capable;
        NO_CAPABILITIES.store(true, Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let mut w =
            Writer::open(dir.path(), WriterConfig::default().segment_size(1 << 20)).unwrap();
        append_one(&mut w);
        w.checkpoint().unwrap();
        let mut r = Reader::open(
            dir.path(),
            ReaderConfig::default()
                .durable_only(true)
                .backoff(blocking(Duration::ZERO)),
        )
        .unwrap();
        assert_eq!(r.try_next().unwrap().unwrap().seq, 0);
        let begins = BEGINS[0].load(Ordering::SeqCst);
        let waits = WAITS.load(Ordering::SeqCst);
        assert!(
            r.next_timeout(Some(Duration::from_millis(500)))
                .unwrap()
                .is_none()
        );
        // Registered on the record words and blocked, up to the 100 ms cap each time: a handful of waits, not a loop
        // that never registers (the unregistered branch with a sleep of zero).
        let begins = BEGINS[0].load(Ordering::SeqCst) - begins;
        let waits = WAITS.load(Ordering::SeqCst) - waits;
        assert!(
            (2..=20).contains(&begins),
            "{begins} registrations in 500 ms"
        );
        assert!((2..=20).contains(&waits), "{waits} waits in 500 ms");
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
    use super::{
        CHUNK, Reader, SlotOutcome, TAIL_CHUNK, TAIL_FILLS, TAIL_PROBE, VALIDATION_RETRIES,
        VALIDATION_WINDOW, resize_buf,
    };
    use crate::mmap::MAPPED_READS;
    use crate::{CorruptReason, Error, NewRecord, ReaderConfig, Writer, WriterConfig};
    use std::cell::Cell;
    use std::os::unix::fs::FileExt;
    use std::time::{Duration, Instant};

    /// The layout of [`Reader`] (ADR 0057, issue #86): the alignment, and the offsets of the fields the per-record path
    /// of either reader reads or writes. 64-bit Linux and macOS only: the offsets depend on the sizes of `PathBuf`, `Vec`
    /// and `Instant`.
    #[test]
    #[cfg(all(
        target_pointer_width = "64",
        any(target_os = "linux", target_os = "macos")
    ))]
    fn reader_layout() {
        use super::{Found, OpenSegment};
        use std::mem::{align_of, offset_of, size_of};
        const WHY: &str = "the per-record path of both readers reads and writes these fields, and the layout of `Reader` \
            is fixed on purpose (ADR 0057, issue #86): a 16-byte access to `found` that straddles a page boundary made \
            replay 15 to 30 % slower on Linux, and moving the hot fields cost 2 to 3 % on x86-64. Add new fields at the \
            end of the struct, or behind a pointer. If a hot field really must move, keep `found` at a multiple of 16, \
            run the replay benchmark before and after on x86-64 and aarch64 (ADR 0057 says how), and update this test \
            and the ADR";
        let seg = offset_of!(Reader, seg);
        let found = offset_of!(Reader, found);
        let hot = [
            ("seg.size", seg + offset_of!(OpenSegment, size), 56),
            ("seg.map", seg + offset_of!(OpenSegment, map), 80),
            ("buf", offset_of!(Reader, buf), 104),
            (
                "config.durable_only",
                offset_of!(Reader, config) + offset_of!(ReaderConfig, durable_only),
                280,
            ),
            ("waiting_since", offset_of!(Reader, waiting_since), 304),
            ("offset", offset_of!(Reader, offset), 328),
            ("next_seq", offset_of!(Reader, next_seq), 336),
            (
                "durable_next_seq",
                offset_of!(Reader, durable_next_seq),
                344,
            ),
            ("bound", offset_of!(Reader, bound), 368),
            ("skip_until", offset_of!(Reader, skip_until), 376),
            ("buf_start", offset_of!(Reader, buf_start), 392),
            ("found.header", found + offset_of!(Found, header), 400),
            (
                "found.payload_at",
                found + offset_of!(Found, payload_at),
                432,
            ),
            (
                "found.payload_len",
                found + offset_of!(Found, payload_len),
                440,
            ),
        ];
        for (field, at, expected) in hot {
            assert_eq!(at, expected, "`Reader::{field}` is at offset {at}, not {expected}: {WHY}");
        }
        // With `found` at a multiple of 16 in a 16-byte aligned struct, the record path's 16-byte accesses to it (at +0,
        // +16 and +32) cannot straddle a page boundary, wherever the reader is.
        assert_eq!(align_of::<Reader>(), 16, "`Reader` is no longer 16-byte aligned: {WHY}");
        assert_eq!(found % 16, 0, "`Reader::found` is not at a multiple of 16: {WHY}");
        assert_eq!(size_of::<Found>(), 48, "`Found` changed size: {WHY}");
        assert_eq!(size_of::<OpenSegment>(), 80, "`OpenSegment` changed size: {WHY}");
    }

    thread_local! {
        /// How many of this thread's next non-empty slot reads [`scripted`] turns into a validation failure.
        static FAIL_READS: Cell<u32> = const { Cell::new(0) };
    }

    thread_local! {
        /// The size of each fill of a `pread` buffer by this thread ([`note_fill`]).
        static FILLS: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Test builds only: a `pread` buffer is about to be filled with `len` bytes.
    pub(super) fn note_fill(len: u64) {
        FILLS.with(|f| f.borrow_mut().push(len));
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

    /// A writer and a safe reader of the same journal, for the tests of the read sizes ([`Reader::fill_size`]).
    struct Follow {
        writer: Writer,
        reader: Reader,
        /// A zero-copy reader, which must return the same records.
        mapped: Reader,
        dir: tempfile::TempDir,
        appended: u64,
    }

    impl Follow {
        fn new(segment_size: u64) -> Follow {
            let dir = tempfile::tempdir().unwrap();
            let writer = Writer::open(
                dir.path(),
                WriterConfig::default().segment_size(segment_size),
            )
            .unwrap();
            let reader = open_reader(dir.path(), false);
            // SAFETY: only the tickertape writer above modifies the journal.
            let mapped =
                unsafe { Reader::open_mapped(dir.path(), ReaderConfig::default()) }.unwrap();
            FILLS.with(|f| f.borrow_mut().clear());
            let mut follow = Follow {
                writer,
                reader,
                mapped,
                dir,
                appended: 0,
            };
            follow.at_once(64);
            follow
        }

        /// The payload of record `seq`.
        fn payload(seq: u64, len: usize) -> Vec<u8> {
            (0..len).map(|i| (seq as usize * 31 + i) as u8).collect()
        }

        fn append(&mut self, count: usize, len: usize) {
            for _ in 0..count {
                self.writer
                    .append(&NewRecord {
                        schema_id: 9,
                        venue_ts_ns: 1,
                        local_ts_ns: 2,
                        payload: &Follow::payload(self.appended, len),
                    })
                    .unwrap();
                self.appended += 1;
            }
        }

        /// Reads `count` records of `len` bytes and returns the sizes of the fills that took.
        fn read(&mut self, count: usize, len: usize) -> Vec<u64> {
            for _ in 0..count {
                let record = self.reader.try_next().unwrap().expect("record missing");
                assert_eq!(record.payload, Follow::payload(record.seq, len));
                let mapped = self.mapped.try_next().unwrap().expect("record missing");
                assert_eq!(record, mapped);
            }
            FILLS.with(|f| std::mem::take(&mut *f.borrow_mut()))
        }

        fn poll_empty(&mut self) {
            assert!(self.reader.try_next().unwrap().is_none());
        }

        /// One record of `len` bytes, appended after the reader found its slot empty: the size of its fill.
        fn follow(&mut self, len: usize) -> u64 {
            self.poll_empty();
            self.append(1, len);
            let fills = self.read(1, len);
            assert_eq!(fills.len(), 1);
            fills[0]
        }

        /// One record of `len` bytes that is there when the reader looks for it, after a slot that was not empty: its
        /// fill is a whole chunk, and the count starts over.
        fn at_once(&mut self, len: usize) {
            self.append(1, len);
            let left = self.reader.seg.size - self.reader.offset;
            assert_eq!(self.read(1, len), [left.min(CHUNK as u64)]);
        }

        /// Follows until the fills are tail reads, which takes exactly `TAIL_FILLS` fills after one that did not follow.
        fn shrink(&mut self) {
            for _ in 1..TAIL_FILLS {
                assert_eq!(self.follow(64), CHUNK as u64);
            }
            assert_eq!(self.follow(64), TAIL_CHUNK as u64);
        }
    }

    /// The safe reader reads little when it follows the writer with time to spare (it read 64 KiB for every record), and
    /// whole chunks otherwise.
    #[test]
    fn the_read_size_shrinks_at_the_tail_and_grows_at_the_first_fill_that_does_not_follow() {
        let mut f = Follow::new(1 << 20);
        // Replay: whole chunks. A chunk holds 630 of these records of 104 bytes.
        f.append(1_000, 64);
        assert_eq!(f.read(1_000, 64), [CHUNK as u64; 2]);
        // The last chunk was used beyond a tail read, so the first fill at the tail does not follow.
        assert_eq!(f.follow(64), CHUNK as u64);
        f.shrink();
        // One fill in `TAIL_PROBE` reads a whole chunk (the one after `TAIL_PROBE - 1` tail reads), and the reads stay tail
        // reads as long as the reader still waits after it.
        for cycle in 0..3 {
            // The first cycle's first tail read was the last fill of `shrink`.
            for _ in u16::from(cycle == 0) + 1..TAIL_PROBE {
                assert_eq!(f.follow(64), TAIL_CHUNK as u64);
            }
            assert_eq!(f.follow(64), CHUNK as u64);
        }
        for _ in 0..50 {
            assert_eq!(f.follow(64), TAIL_CHUNK as u64);
        }
        // Several records at once, that a tail read holds: the reader did not wait for each, so the next fill is a whole
        // chunk, and the count starts over.
        f.poll_empty();
        f.append(39, 64);
        assert_eq!(f.read(39, 64), [TAIL_CHUNK as u64]);
        assert_eq!(f.follow(64), CHUNK as u64);
        f.shrink();
        // A record that was there at once (after a whole chunk, as when a probe found that the reader no longer waits):
        // whole chunks again, and the count starts over.
        f.at_once(64);
        f.shrink();
        // A backlog: the tail read is consumed, and the rest is read in whole chunks (the 40th record crosses its end).
        f.poll_empty();
        f.append(1_000, 64);
        assert_eq!(
            f.read(1_000, 64),
            [TAIL_CHUNK, CHUNK, CHUNK].map(|n| n as u64)
        );
        assert_eq!(f.follow(64), CHUNK as u64);
        // Fills that follow now and then, as a blocking reader's do that finds every second record at once: never tail
        // reads.
        for _ in 0..4 * TAIL_FILLS {
            assert_eq!(f.follow(64), CHUNK as u64);
            f.at_once(64);
        }
        // Batches: never tail reads, though each is waited for.
        for _ in 0..4 * TAIL_FILLS {
            f.poll_empty();
            f.append(2, 64);
            assert_eq!(f.read(2, 64), [CHUNK as u64]);
        }
    }

    /// A record larger than a tail read is read whole, and the reads stay tail reads.
    #[test]
    fn a_record_larger_than_a_tail_read_is_read_whole() {
        let mut f = Follow::new(1 << 20);
        f.shrink();
        for _ in 0..4 * TAIL_FILLS {
            assert_eq!(f.follow(10_000), 10_040);
        }
        assert_eq!(f.follow(64), TAIL_CHUNK as u64);
        // Larger than a whole chunk.
        assert_eq!(f.follow(100_000), 100_040);
        assert_eq!(f.follow(64), TAIL_CHUNK as u64);
        // The second record crosses the end of the tail read that began with the first one, and is there when the reader
        // looks for it.
        f.poll_empty();
        f.append(1, 4_000);
        f.append(1, 200);
        assert_eq!(f.read(1, 4_000), [TAIL_CHUNK as u64]);
        assert_eq!(f.read(1, 200), [CHUNK as u64]);
        // Records larger than a tail read are followed with whole chunks until the reads have shrunk.
        for _ in 1..TAIL_FILLS {
            assert_eq!(f.follow(10_000), CHUNK as u64);
        }
        assert_eq!(f.follow(10_000), 10_040);
    }

    /// The first fill in a segment is a whole chunk, and reads never reach past a segment.
    #[test]
    fn a_segment_roll_starts_over_with_whole_chunks() {
        for waits in [false, true] {
            let mut f = Follow::new(1 << 16);
            let base = f.reader.seg.base;
            let mut tail_reads = 0;
            for _ in 0..4 {
                let before = f.reader.seg.base;
                loop {
                    f.poll_empty();
                    f.append(1, 64);
                    let left = f.reader.seg.size - f.reader.offset;
                    let mut fills = f.read(1, 64);
                    if f.reader.seg.base != before {
                        // The seal's fill, at most to the end of the old segment, and the new segment's.
                        assert_eq!(fills.remove(0), left.min(TAIL_CHUNK as u64));
                        assert_eq!(fills, [CHUNK as u64 - 64]);
                        break;
                    }
                    assert_eq!(fills.len(), 1);
                    tail_reads += usize::from(fills[0] == left.min(TAIL_CHUNK as u64));
                    assert!(fills[0] <= left);
                }
                if waits {
                    f.poll_empty();
                }
            }
            assert_ne!(f.reader.seg.base, base);
            assert!(tail_reads > 4 * 500, "{tail_reads}");
        }
    }

    /// Seeks start over with whole chunks, wherever they land and whatever the reader did before.
    #[test]
    fn seeks_start_over_with_whole_chunks() {
        let mut f = Follow::new(1 << 20);
        f.shrink();
        let polled_before = |f: &mut Follow, polls: u32| {
            for _ in 0..polls {
                f.poll_empty();
            }
        };
        for polls in [0, 1] {
            // Back.
            polled_before(&mut f, polls);
            f.append(1, 64);
            f.reader.seek(3).unwrap();
            f.mapped.seek(3).unwrap();
            let left = f.appended as usize - 3;
            assert_eq!(f.read(left, 64), [CHUNK as u64]);
            f.at_once(64);
            f.shrink();
            // To the record that comes next.
            polled_before(&mut f, polls);
            f.append(1, 64);
            let at = f.reader.position();
            f.reader.seek(at).unwrap();
            f.mapped.seek(at).unwrap();
            assert_eq!(f.read(1, 64), [CHUNK as u64]);
            f.at_once(64);
            f.shrink();
            // To the end: the records before it are read in whole chunks, and the count starts over there.
            f.append(5, 64);
            f.reader.seek_to_end().unwrap();
            f.mapped.seek_to_end().unwrap();
            assert_eq!(f.read(0, 64), [CHUNK as u64]);
            assert_eq!(f.follow(64), CHUNK as u64);
            f.at_once(64);
            f.shrink();
        }
        // A wait right after a seek is still no reason for a tail read (the seek emptied the buffer). To the end, with the marker at the end:
        f.writer.checkpoint().unwrap();
        f.reader.seek_to_end().unwrap();
        f.mapped.seek_to_end().unwrap();
        f.read(0, 64);
        f.poll_empty();
        f.append(1, 64);
        assert_eq!(f.read(1, 64), [CHUNK as u64]);
        f.at_once(64);
        f.shrink();
        // To the record that comes next, which the marker covers the way to:
        let at = f.reader.position();
        f.writer.checkpoint().unwrap();
        f.reader.seek(at).unwrap();
        f.mapped.seek(at).unwrap();
        f.poll_empty();
        f.read(0, 64);
        f.append(1, 64);
        assert_eq!(f.read(1, 64), [CHUNK as u64]);
    }

    /// A reader that blocked for its records (registered as a waiter, so that the writer made a wake-up call for each)
    /// reads whole chunks, however long it waited: the rule has no time in it. Blocks are simulated as `block` makes them:
    /// the wake-up region is taken out of the waker while `block_on` reads the slot again, and that read either finds the
    /// slot still empty or finds the record. A reader that stops blocking counts again from its first fill that follows.
    #[test]
    fn a_reader_that_blocked_reads_whole_chunks() {
        /// One record that the reader blocked for: `in_check` if `block_on`'s own check found it, otherwise after the slot
        /// was found empty there too. Returns the size of its fill.
        fn blocked(f: &mut Follow, in_check: bool) -> u64 {
            f.poll_empty();
            let mut region = f.reader.waker.as_mut().unwrap().take();
            if !in_check {
                // Still empty in `block_on`'s check: the reader waits, wakes, and reads with the region put back.
                f.poll_empty();
                let wait = f.reader.wait.tail_wait.unwrap();
                assert!(wait.attached && wait.blocked);
                f.reader.waker.as_mut().unwrap().put(region.take().unwrap());
            }
            f.append(1, 64);
            let fills = f.read(1, 64);
            if let Some(region) = region {
                f.reader.waker.as_mut().unwrap().put(region);
            }
            assert_eq!(fills.len(), 1);
            fills[0]
        }
        let mut f = Follow::new(1 << 20);
        let waker = f.reader.waker.as_ref();
        assert!(waker.is_some_and(crate::notify::Waker::attached));
        // Blocked, whichever way the wait ended: never tail reads.
        for i in 0..8 * TAIL_FILLS {
            assert_eq!(blocked(&mut f, i % 3 == 0), CHUNK as u64);
        }
        // A reader that stops blocking counts from its first fill that follows.
        for _ in 1..TAIL_FILLS {
            assert_eq!(f.follow(64), CHUNK as u64);
        }
        assert_eq!(f.follow(64), TAIL_CHUNK as u64);
        // One block ends the tail reads, in either way, and the count starts over.
        assert_eq!(blocked(&mut f, false), CHUNK as u64);
        f.shrink();
        assert_eq!(blocked(&mut f, true), CHUNK as u64);
        f.shrink();
        // Without a region, a reader finds slots empty only "unattached": that is not a block.
        f.reader.waker = None;
        for _ in 0..4 {
            assert_eq!(f.follow(64), TAIL_CHUNK as u64);
        }
        assert!(!f.reader.wait.tail_wait.unwrap().blocked);
    }

    /// A reader that blocks on the wake-up region is noted as blocked at the slot (the check inside `block_on` reads it
    /// with the region taken out), however the wait ends. The timeout is long against anything the first poll does
    /// before the reader blocks (a marker read and a directory listing), so that a slow machine still reaches the block.
    #[test]
    fn blocking_on_the_region_is_noted() {
        let mut f = Follow::new(1 << 20);
        let config = ReaderConfig::default().backoff(crate::config::Backoff {
            spins: 0,
            yields: 0,
            min_sleep: Duration::from_millis(1),
            max_sleep: Duration::from_millis(1),
        });
        let mut reader = Reader::open(f.dir.path(), config).unwrap();
        assert!(reader.try_next().unwrap().is_some());
        assert!(
            reader
                .next_timeout(Some(Duration::from_millis(100)))
                .unwrap()
                .is_none()
        );
        let wait = reader.wait.tail_wait.expect("the slot was found empty");
        assert_eq!((wait.base, wait.offset), (reader.seg.base, reader.offset));
        assert!(wait.attached && wait.blocked, "{wait:?}");
        f.append(1, 64);
        assert!(reader.try_next().unwrap().is_some());
    }

    /// A durable-only reader reads whole chunks, also when it waits at the checkpoint for every record: each marker
    /// refresh empties the buffer.
    #[test]
    fn a_marker_refresh_reads_whole_chunks() {
        let mut f = Follow::new(1 << 20);
        let mut reader =
            Reader::open(f.dir.path(), ReaderConfig::default().durable_only(true)).unwrap();
        f.writer.checkpoint().unwrap();
        assert!(reader.try_next().unwrap().is_some());
        FILLS.with(|f| f.borrow_mut().clear());
        for round in 0..3 * u64::from(TAIL_FILLS) {
            assert!(reader.try_next().unwrap().is_none());
            f.append(2, 64);
            assert!(reader.try_next().unwrap().is_none());
            f.writer.checkpoint().unwrap();
            for seq in [2 * round + 1, 2 * round + 2] {
                let record = reader.try_next().unwrap().unwrap();
                assert_eq!(record.payload, Follow::payload(seq, 64));
            }
            assert!(reader.try_next().unwrap().is_none());
            let fills = FILLS.with(|f| std::mem::take(&mut *f.borrow_mut()));
            assert_eq!(fills, [CHUNK as u64]);
        }
    }

    /// A record that fails validation in a tail read is read again, as a whole chunk.
    #[test]
    fn a_slot_read_again_is_read_as_a_whole_chunk() {
        let mut f = Follow::new(1 << 20);
        f.shrink();
        f.poll_empty();
        f.append(1, 64);
        FAIL_READS.with(|n| n.set(1));
        assert_eq!(f.read(1, 64), [TAIL_CHUNK, CHUNK].map(|n| n as u64));
        assert_eq!(FAIL_READS.with(Cell::get), 0);
        assert_eq!(f.follow(64), CHUNK as u64);
    }

    /// Whatever the mix of waits, batches and sizes, over many rolls and seeks, the safe reader returns the records the
    /// zero-copy reader returns, and both kinds of reads occur.
    #[test]
    fn mixed_read_sizes_return_the_same_records() {
        let mut f = Follow::new(1 << 18);
        let mut rng = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        let (mut small, mut whole) = (0, 0);
        for round in 0..3_000 {
            for _ in 0..next(3) {
                f.poll_empty();
            }
            // Long stretches of one record at a time, so that the reads shrink now and then.
            let (count, len) = match next(60) {
                0 => (next(200) as usize + 1, next(300) as usize),
                1 => (1, next(30_000) as usize),
                2 => (2, next(5_000) as usize),
                _ => {
                    f.poll_empty();
                    (1, next(100) as usize)
                }
            };
            f.append(count, len);
            for fill in f.read(count, len) {
                small += usize::from(fill == TAIL_CHUNK as u64);
                whole += usize::from(fill == CHUNK as u64);
            }
            if round % 500 == 499 {
                let at = f.appended - 1;
                f.reader.seek(at).unwrap();
                f.mapped.seek(at).unwrap();
                assert!(!f.read(1, len).contains(&(TAIL_CHUNK as u64)));
            }
        }
        assert!(small > 100 && whole > 100, "{small} {whole}");
    }

    /// A failed `seek_to_end` puts back every field that describes the reader's place, the timers of a wait and the last
    /// record passed included (no observable behaviour depends on each of these alone, so they are compared here).
    #[test]
    fn a_failed_seek_to_end_restores_every_field() {
        let dir = journal_of(1_000);
        let mut newest: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "tape"))
            .collect();
        newest.sort();
        assert!(newest.len() > 1);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(newest.last().unwrap())
            .unwrap();
        // The second record of the newest segment (140-byte frames), so that the call passes one first.
        file.write_all_at(&[0xEE; 8], 64 + 144 + 9).unwrap();
        let state = |r: &Reader| {
            (
                (r.seg.path.clone(), r.seg.identity, r.offset, r.next_seq),
                (r.skip_until, r.seek_covered, r.buf.clone(), r.buf_start),
                (r.durable_next_seq, r.durable_segment_base),
                (r.durable_offset, r.bound),
                (
                    (r.waiting_since, *r.wait),
                    r.superseded_checked,
                    format!("{:?}", r.found),
                ),
            )
        };
        for mapped in [false, true] {
            let mut reader = open_reader(dir.path(), mapped);
            for _ in 0..5 {
                reader.try_next().unwrap().unwrap();
            }
            reader.seek_covered = 3;
            reader.durable_next_seq = 7;
            reader.durable_segment_base = 8;
            reader.durable_offset = 9;
            reader.bound = 1 << 12;
            reader.wait.tail_fills = 5;
            reader.wait.tail_wait = Some(super::SlotWait {
                base: 1,
                offset: 2,
                attached: true,
                blocked: true,
            });
            reader.waiting_since = Some(Instant::now());
            reader.wait.listed = Some(super::Listed {
                at: Instant::now(),
                unchanged: None,
            });
            reader.superseded_checked = Some(Instant::now() - VALIDATION_WINDOW);
            let before = state(&reader);
            assert!(matches!(reader.seek_to_end(), Err(Error::Corrupt { .. })));
            assert_eq!(state(&reader), before, "mapped: {mapped}");
            assert_eq!(reader.try_next().unwrap().unwrap().seq, 5);
        }
    }

    /// A journal whose only sealed segment, of `size` bytes, has an index with one entry, and the entry.
    #[cfg(feature = "fault-injection")]
    fn indexed_journal(size: u64) -> (tempfile::TempDir, crate::seek_index::codec::Entry) {
        use crate::format;
        let dir = tempfile::tempdir().unwrap();
        let path = crate::journal::segment_path(dir.path(), 0);
        let file = std::fs::File::create_new(&path).unwrap();
        file.set_len(size).unwrap(); // Sparse.
        let header = format::encode_segment_header(&format::SegmentHeader {
            segment_size: size,
            base_seq: 0,
            created_unix_ns: 3,
        });
        file.write_all_at(&header, 0).unwrap();
        let (mut at, mut entry) = (format::SEGMENT_HEADER_LEN as u64, None);
        for seq in 0..1_000 {
            let mut frame = [0; 104];
            let len = frame.len() as u32;
            let fields = crate::format::RecordHeader {
                seq,
                schema_id: 0,
                venue_ts_ns: 0,
                local_ts_ns: 0,
            };
            format::encode_record_body(&fields, &[0x11; 64], &mut frame[4..]);
            frame[..4].copy_from_slice(&len.to_le_bytes());
            file.write_all_at(&frame, at).unwrap();
            if seq == 700 {
                entry = Some(crate::seek_index::codec::Entry {
                    seq,
                    offset: at,
                    len,
                    crc: u32::from_le_bytes(frame[4..8].try_into().unwrap()),
                });
            }
            at += u64::from(len);
        }
        file.write_all_at(&format::SEAL_MARKER.to_le_bytes(), at)
            .unwrap();
        file.write_all_at(&format::seal_crc(1_000).to_le_bytes(), at + 4)
            .unwrap();
        let entry = entry.unwrap();
        let mut encoder = crate::seek_index::codec::Encoder::with_spacing(4_096);
        encoder.push(entry);
        let index = encoder.finish(&header, at, 1_000).unwrap();
        let indexes = dir.path().join(crate::seek_index::DIR_NAME);
        std::fs::create_dir(&indexes).unwrap();
        std::fs::write(indexes.join("00000000000000000000.idx"), index).unwrap();
        // A successor and a marker in it, for mapped and durable-only readers.
        std::fs::write(
            crate::journal::segment_path(dir.path(), 1_000),
            [
                &format::encode_segment_header(&format::SegmentHeader {
                    segment_size: 65_536,
                    base_seq: 1_000,
                    created_unix_ns: 4,
                })[..],
                &[0; 65_536 - 64],
            ]
            .concat(),
        )
        .unwrap();
        let slot = format::encode_checkpoint_slot(&format::CheckpointSlot {
            generation: 1,
            next_seq: 1_000,
            segment_base_seq: 1_000,
            offset: 64,
        });
        std::fs::write(dir.path().join("checkpoint"), slot).unwrap();
        (dir, entry)
    }

    #[cfg(feature = "fault-injection")]
    fn open_indexed(dir: &std::path::Path, mapped: bool) -> Reader {
        let config = ReaderConfig::default().seek_index(true);
        if mapped {
            // SAFETY: nothing but this test changes the journal, and it holds no payload.
            unsafe { Reader::open_mapped(dir, config) }.unwrap()
        } else {
            Reader::open(dir, config).unwrap()
        }
    }

    /// 8a before 8b (ADR 0045, test plan 4): where an index names bytes inside a payload of another history that decode
    /// to a length near the segment's size, nothing is allocated or read for a frame; nor where the commit word changes
    /// to one between 8a and 8b.
    #[test]
    #[cfg(feature = "fault-injection")]
    fn a_length_that_was_not_compared_sizes_no_buffer() {
        for size in [1 << 20, 4 << 30] {
            for word in [0xF000_0000_u32, (size - 100_000).min(0xFFFF_FFFE) as u32, 0] {
                let (dir, entry) = indexed_journal(size);
                let path = crate::journal::segment_path(dir.path(), 0);
                let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
                for mapped in [false, true] {
                    let mut reader = open_indexed(dir.path(), mapped);
                    reader.seek(900).unwrap();
                    assert!(reader.used_index, "{size}, mapped {mapped}");
                    assert_eq!(reader.offset, entry.offset);
                    assert_eq!(reader.try_next().unwrap().unwrap().seq, 900);
                    // Found by 8a.
                    let write = |word: u32| {
                        file.write_all_at(&word.to_le_bytes(), entry.offset)
                            .unwrap();
                    };
                    write(word);
                    let mut reader = open_indexed(dir.path(), mapped);
                    reader.seek(900).unwrap();
                    assert!(!reader.used_index);
                    assert_eq!((reader.offset, reader.buf.capacity()), (64, 0));
                    write(entry.len);
                    // Another record of the same length: 8a sees its checksum, before the frame is read.
                    let crc = |crc: u32| {
                        file.write_all_at(&crc.to_le_bytes(), entry.offset + 4)
                            .unwrap();
                    };
                    crc(entry.crc ^ 1);
                    let mut reader = open_indexed(dir.path(), mapped);
                    reader.seek(900).unwrap();
                    assert!(!reader.used_index);
                    assert_eq!((reader.offset, reader.buf.capacity()), (64, 0));
                    crc(entry.crc);
                    // Changed after 8a.
                    let mut reader = open_indexed(dir.path(), mapped);
                    let other = file.try_clone().unwrap();
                    crate::fault::clear();
                    crate::fault::run_at(
                        crate::fault::Point::ReaderSeekIndexLanding,
                        0,
                        move || {
                            other
                                .write_all_at(&word.to_le_bytes(), entry.offset)
                                .unwrap();
                        },
                    );
                    reader.seek(900).unwrap();
                    assert!(!reader.used_index);
                    assert_eq!((reader.offset, reader.buf.capacity()), (64, 0));
                    write(entry.len);
                }
            }
        }
    }

    /// 8b fails where the file ends inside the record by then (no panic, no error, no landing), and where it holds another
    /// valid record with the same sequence number and length (its stored CRC is not the entry's), or the record with its
    /// stored CRC damaged.
    #[test]
    #[cfg(feature = "fault-injection")]
    fn eight_b_reads_the_record_that_was_indexed_or_nothing() {
        let (dir, entry) = indexed_journal(1 << 20);
        let path = crate::journal::segment_path(dir.path(), 0);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        let image = std::fs::read(crate::journal::segment_path(dir.path(), 0)).unwrap();
        let mut other = vec![0; entry.len as usize];
        let fields = crate::format::RecordHeader {
            seq: entry.seq,
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns: 0,
        };
        crate::format::encode_record_body(&fields, &[0x22; 64], &mut other[4..]);
        other[..4].copy_from_slice(&entry.len.to_le_bytes());
        let scan = |reader: &mut Reader| {
            let mut got = Vec::new();
            while let Ok(Some(record)) = reader.next_owned() {
                got.push(record);
            }
            got
        };
        // A mapped reader must not meet a truncated file.
        let cases = [0, 2, 4, 50]
            .map(|cut| (false, Some(entry.offset + cut), 0))
            .into_iter()
            .chain(
                [104, 8]
                    .into_iter()
                    .flat_map(|n| [(false, None, n), (true, None, n)]),
            );
        for (mapped, cut, len) in cases {
            let mut reader = open_indexed(dir.path(), mapped);
            let (at, mut frame) = (file.try_clone().unwrap(), other.clone());
            frame.truncate(len);
            crate::fault::clear();
            crate::fault::run_at(
                crate::fault::Point::ReaderSeekIndexLanding,
                0,
                move || match cut {
                    Some(len) => at.set_len(len).unwrap(),
                    None => at.write_all_at(&frame, entry.offset).unwrap(),
                },
            );
            reader.seek(900).unwrap();
            assert!(!reader.used_index, "mapped {mapped}, cut {cut:?}, {len}");
            assert_eq!((reader.offset, reader.next_seq), (64, 0));
            assert!(reader.buf.is_empty());
            file.set_len(image.len() as u64).unwrap();
            file.write_all_at(&image, 0).unwrap();
            let mut plain = open_reader(dir.path(), mapped);
            plain.seek(900).unwrap();
            let want = scan(&mut plain);
            assert_eq!(want.len(), 100);
            assert_eq!(scan(&mut reader), want);
        }
    }

    /// A mapped reader reads nothing into its buffer but the landing record: it does not keep it.
    #[test]
    #[cfg(feature = "fault-injection")]
    fn a_mapped_reader_does_not_keep_the_landing_record() {
        let (dir, _) = indexed_journal(1 << 20);
        let mut reader = open_indexed(dir.path(), true);
        reader.seek(900).unwrap();
        assert!(reader.used_index);
        if MAPPED_READS {
            assert_eq!(reader.buf.capacity(), 0);
        }
        assert_eq!(reader.try_next().unwrap().unwrap().seq, 900);
    }

    /// A seek that lands through an index forgets what an earlier seek knew to be covered, and empties the buffer (step
    /// 9); one whose index fails sets it as without an index. Neither touches the checkpoint a durable-only reader has
    /// applied.
    #[test]
    #[cfg(feature = "fault-injection")]
    fn a_landing_leaves_nothing_a_later_seek_trusts() {
        for mapped in [false, true] {
            let (dir, _) = indexed_journal(1 << 20);
            let mut reader = open_indexed(dir.path(), mapped);
            reader.durable_next_seq = 5;
            reader.seek(0).unwrap();
            assert_eq!(reader.seek_covered, 1_000);
            reader.try_next().unwrap().unwrap();
            reader.seek(900).unwrap();
            assert!(!reader.used_index, "too close");
            reader.seek_covered = 0;
            reader.seek(900).unwrap();
            assert!(reader.used_index);
            assert_eq!(
                (reader.seek_covered, reader.next_seq, reader.skip_until),
                (0, 700, 900)
            );
            assert!(reader.buf.is_empty());
            assert_eq!(reader.durable_next_seq, 5);
            std::fs::remove_dir_all(dir.path().join(crate::seek_index::DIR_NAME)).unwrap();
            reader.seek(900).unwrap();
            assert!(!reader.used_index);
            assert_eq!((reader.seek_covered, reader.offset), (1_000, 64));
        }
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
