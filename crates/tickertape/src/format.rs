//! The on-disk format (version 1), as pure functions over precisely bounded byte slices.
//!
//! This module mirrors `docs/format.md` and is the only place that knows byte offsets. It never touches files or
//! memory maps and contains no `unsafe`, so it runs under Miri and is the target of the fuzzers. Most users never
//! need it; it is public for tools that decode journals themselves.
//!
//! Live mappings are shared with other processes, so (ADR 0003) nothing here takes a slice that covers a commit
//! word or a whole live segment. Single-record functions take the commit word as a value and the record **body**
//! (`[P+4, P+len)`) as a slice. Whole-segment functions go through [`SegmentView`], which an in-memory image and the
//! crate's mapping wrapper both implement.

use crate::error::CorruptReason;

/// The 8 magic bytes at the start of every segment: ASCII `TICKTAPE`.
pub const MAGIC: [u8; 8] = *b"TICKTAPE";
/// The 8 magic bytes at the start of each checkpoint marker slot: ASCII `TICKCKPT`.
pub const CHECKPOINT_MAGIC: [u8; 8] = *b"TICKCKPT";
/// The format version this build reads and writes.
pub const FORMAT_VERSION: u16 = 1;
/// Length of the segment header, and the offset of the first record.
pub const SEGMENT_HEADER_LEN: usize = 64;
/// Length of a record header (commit word included).
pub const RECORD_HEADER_LEN: usize = 40;
/// Bytes at the end of every segment that never hold a record, so that a seal always fits.
pub const SEGMENT_TRAILER_LEN: usize = 8;
/// Length of a seal (commit word included).
pub const SEAL_LEN: usize = 8;
/// Length of the commit word.
pub const COMMIT_WORD_LEN: usize = 4;
/// Commit word value marking a seal.
pub const SEAL_MARKER: u32 = u32::MAX;
/// Largest valid record frame length.
pub const MAX_FRAME_LEN: u32 = u32::MAX - 1;
/// Records and seals start at multiples of this.
pub const RECORD_ALIGN: usize = 8;
/// `segment_size` must be a multiple of this, and at least this.
pub const SEGMENT_SIZE_UNIT: u64 = 64 * 1024;
/// Largest allowed `segment_size`.
pub const MAX_SEGMENT_SIZE: u64 = 1 << 40;
/// File name extension of segments.
pub const SEGMENT_EXTENSION: &str = "tape";
/// Size of the checkpoint marker file.
pub const CHECKPOINT_FILE_LEN: usize = 131_072;
/// Offsets of the two checkpoint slots (64 KiB apart: different filesystem blocks for block sizes up to 64 KiB).
pub const CHECKPOINT_SLOT_OFFSETS: [usize; 2] = [0, 65_536];
/// Length of one checkpoint slot.
pub const CHECKPOINT_SLOT_LEN: usize = 64;

/// A decoded segment header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentHeader {
    /// Total file size in bytes.
    pub segment_size: u64,
    /// Sequence number of the segment's first record.
    pub base_seq: u64,
    /// Writer's wall-clock time at creation (informational).
    pub created_unix_ns: u64,
}

/// The fields of a record header other than the commit word, CRC and reserved flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordHeader {
    /// Sequence number.
    pub seq: u64,
    /// Schema id.
    pub schema_id: u32,
    /// Venue timestamp.
    pub venue_ts_ns: u64,
    /// Local timestamp.
    pub local_ts_ns: u64,
}

/// A decoded checkpoint marker slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointSlot {
    /// Incremented on every checkpoint; the higher valid one is current.
    pub generation: u64,
    /// Every record with a smaller sequence number was durable when the slot was written.
    pub next_seq: u64,
    /// The segment containing the position of `next_seq`.
    pub segment_base_seq: u64,
    /// Offset of `next_seq`'s record, or of a seal, in that segment.
    pub offset: u64,
}

/// Checks that `segment_size` satisfies the format's constraints.
pub fn validate_segment_size(segment_size: u64) -> Result<(), CorruptReason> {
    let _ = segment_size;
    todo!("M2")
}

/// Largest payload, in bytes, that fits in a segment of `segment_size` bytes:
/// `min(segment_size - 112, MAX_FRAME_LEN - 40)`. `segment_size` must be valid.
pub fn max_payload_len(segment_size: u64) -> u64 {
    let _ = segment_size;
    todo!("M2")
}

/// Rounds up to the next multiple of [`RECORD_ALIGN`].
pub const fn align8(n: u64) -> u64 {
    (n + (RECORD_ALIGN as u64 - 1)) & !(RECORD_ALIGN as u64 - 1)
}

/// True if a record of frame length `len` starting at `offset` fits in the record area of a segment of
/// `segment_size` bytes (`offset + align8(len) <= segment_size - 8`), computed without overflow.
pub fn record_fits(segment_size: u64, offset: u64, len: u32) -> bool {
    let _ = (segment_size, offset, len);
    todo!("M2")
}

/// Encodes a segment header.
pub fn encode_segment_header(header: &SegmentHeader) -> [u8; SEGMENT_HEADER_LEN] {
    let _ = header;
    todo!("M2")
}

/// Decodes and validates a segment header (magic, version, header length, flags, CRC, size constraints).
///
/// Comparing with the actual file size and the file name is the caller's job.
pub fn decode_segment_header(
    buf: &[u8; SEGMENT_HEADER_LEN],
) -> Result<SegmentHeader, CorruptReason> {
    let _ = buf;
    todo!("M2")
}

/// The frame length (commit word value) for a payload of `payload_len` bytes, or `None` if it exceeds
/// [`MAX_FRAME_LEN`].
pub fn frame_len(payload_len: usize) -> Option<u32> {
    let _ = payload_len;
    todo!("M2")
}

/// Writes a record body (`[P+4, P+len)`: CRC, header fields, payload) into `body`, which must be exactly
/// `len - 4` bytes for `len = frame_len(payload.len())`. Returns `len`, which the caller then publishes as the
/// commit word with a release store.
pub fn encode_record_body(header: &RecordHeader, payload: &[u8], body: &mut [u8]) -> u32 {
    let _ = (header, payload, body);
    todo!("M2")
}

/// Fills in the CRC of a body whose header fields and payload are already in place (used after
/// [`write_record_fields`] and an in-place payload fill). `body` is `[P+4, P+len)`.
pub fn finish_record_body(len: u32, body: &mut [u8]) {
    let _ = (len, body);
    todo!("M2")
}

/// Writes the header fields (`seq` through `local_ts_ns`, reserved flags zero) into `body[4..36]`, leaving the CRC
/// and payload alone.
pub fn write_record_fields(header: &RecordHeader, body: &mut [u8]) {
    let _ = (header, body);
    todo!("M2")
}

/// Validates a record body against its commit word and the expected sequence number, and decodes it.
///
/// `commit_word` is the value loaded from `P` (not a seal, not zero); `body` is exactly `[P+4, P+len)`. Bounds against
/// the segment ([`record_fits`]) are the caller's job, because only the caller knows `P`.
pub fn decode_record_body(
    commit_word: u32,
    body: &[u8],
    expected_seq: u64,
) -> Result<(RecordHeader, &[u8]), CorruptReason> {
    let _ = (commit_word, body, expected_seq);
    todo!("M2")
}

/// The `seal_crc` value for a seal announcing `next_seq`.
pub fn seal_crc(next_seq: u64) -> u32 {
    let _ = next_seq;
    todo!("M2")
}

/// Encodes one checkpoint marker slot.
pub fn encode_checkpoint_slot(slot: &CheckpointSlot) -> [u8; CHECKPOINT_SLOT_LEN] {
    let _ = slot;
    todo!("M2")
}

/// Picks the current checkpoint from the marker file's contents: the valid slot with the higher generation, or
/// `None` if neither is valid or `file` is too short.
pub fn decode_checkpoint(file: &[u8]) -> Option<CheckpointSlot> {
    let _ = file;
    todo!("M2")
}

/// Read access to one segment: an in-memory image (`[u8]`), the `pread`-based reader (which copies), or the
/// writer's own mapping (crate-private, constructible only while holding the writer lock).
///
/// Every method is safe for every argument: out-of-range or misaligned requests return `None`. Implementations must
/// order reads so that bytes passed to `with_bytes` after a non-zero `commit_word` at the same record reflect at
/// least everything written before that commit word was stored (ADR 0003). Implementations over live shared memory
/// never hand out slices that another process may still write; they copy, unless (as for the writer) no other
/// process writes the file.
pub trait SegmentView {
    /// Total size in bytes.
    fn len(&self) -> u64;
    /// True if the segment has zero length.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// The commit word at `offset`, with acquire semantics. `None` unless `offset` is a multiple of 8 and
    /// `offset + 4 <= len`.
    fn commit_word(&self, offset: u64) -> Option<u32>;
    /// Calls `f` with the bytes `[offset, offset + len)`. `None` if the range is out of bounds.
    fn with_bytes<R>(&self, offset: u64, len: u64, f: impl FnOnce(&[u8]) -> R) -> Option<R>;
}

impl SegmentView for [u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }

    fn commit_word(&self, offset: u64) -> Option<u32> {
        let _ = offset;
        todo!("M2")
    }

    fn with_bytes<R>(&self, offset: u64, len: u64, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
        let _ = (offset, len, f);
        todo!("M2")
    }
}

/// What [`read_slot`] found at a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// Commit word 0: no record (yet).
    Empty,
    /// A valid seal announcing the expected sequence number.
    Seal,
    /// A valid record.
    Record {
        /// The decoded header fields.
        header: RecordHeader,
        /// Frame length.
        len: u32,
        /// Offset of the next slot.
        next_offset: u64,
    },
}

/// Reads and fully validates the slot at `offset` of a segment, expecting sequence number `expected_seq`
/// (`docs/format.md` → Record, Seal).
pub fn read_slot<V: SegmentView + ?Sized>(
    segment: &V,
    offset: u64,
    expected_seq: u64,
) -> Result<Slot, CorruptReason> {
    let _ = (segment, offset, expected_seq);
    todo!("M2")
}

/// Where a forward scan of one segment stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanEnd {
    /// A zero commit word: the clean end of the written part.
    Empty,
    /// A valid seal.
    Seal,
    /// An invalid record or seal.
    Invalid(CorruptReason),
}

/// The result of [`scan_segment`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanResult {
    /// Offset of the first slot that is not a valid record.
    pub end_offset: u64,
    /// Sequence number the next record would have.
    pub next_seq: u64,
    /// Number of valid records scanned.
    pub records: u64,
    /// What is at `end_offset`.
    pub end: ScanEnd,
}

/// Scans a segment from `start_offset`, expecting `start_seq` there, validating each record, and reports where the
/// valid run ends. Used by writer recovery, `verify` and the fuzzers.
pub fn scan_segment<V: SegmentView + ?Sized>(
    segment: &V,
    start_offset: u64,
    start_seq: u64,
) -> ScanResult {
    let _ = (segment, start_offset, start_seq);
    todo!("M2")
}

/// The file name of the segment whose first record is `base_seq`, e.g. `00000000000000000042.tape`.
pub fn segment_file_name(base_seq: u64) -> String {
    format!("{base_seq:020}.{SEGMENT_EXTENSION}")
}

/// Parses a segment file name back into its `base_seq`. Returns `None` for any other name, including temporary
/// files.
pub fn parse_segment_file_name(name: &str) -> Option<u64> {
    let _ = name;
    todo!("M2")
}
