//! The on-disk format (version 1), as pure functions over byte slices.
//!
//! This module mirrors `docs/format.md` and is the only place that knows byte offsets. It never touches files or
//! memory maps and contains no `unsafe`, so it runs under Miri and is the target of the fuzzers. Most users never
//! need it; it is public for tools that decode journals themselves.
//!
//! Commit words (a record's `len`, or a seal) are *not* read or written here: in a live mapping they must be
//! accessed atomically, which the writer and reader do themselves. Functions here take the commit word as a
//! parameter, or write everything except it.

use crate::error::CorruptReason;

/// The 8 magic bytes at the start of every segment: ASCII `TICKTAPE`.
pub const MAGIC: [u8; 8] = *b"TICKTAPE";
/// The format version this build reads and writes.
pub const FORMAT_VERSION: u16 = 1;
/// Length of the segment header, and the offset of the first record.
pub const SEGMENT_HEADER_LEN: usize = 64;
/// Length of a record header.
pub const RECORD_HEADER_LEN: usize = 40;
/// Bytes reserved at the end of every segment, so that a seal always fits.
pub const SEGMENT_TRAILER_LEN: usize = 8;
/// Length of a seal.
pub const SEAL_LEN: usize = 8;
/// Commit word value marking a seal.
pub const SEAL_MARKER: u32 = u32::MAX;
/// Records and seals start at multiples of this.
pub const RECORD_ALIGN: usize = 8;
/// `segment_size` must be a multiple of this, and at least this.
pub const SEGMENT_SIZE_UNIT: u64 = 64 * 1024;
/// Largest allowed `segment_size`.
pub const MAX_SEGMENT_SIZE: u64 = 1 << 40;
/// File name extension of segments.
pub const SEGMENT_EXTENSION: &str = "tape";

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

/// The fields of a record header other than the commit word and CRC.
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

/// What [`decode_at`] found at a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot<'a> {
    /// Commit word 0: no record (yet).
    Empty,
    /// A valid seal; the next record is the first one of segment `next_seq`.
    Seal {
        /// Sequence number of the next record.
        next_seq: u64,
    },
    /// A valid record.
    Record {
        /// The decoded header fields.
        header: RecordHeader,
        /// The payload, borrowed from the input.
        payload: &'a [u8],
        /// Offset of the next slot (`offset + align8(len)`).
        next_offset: usize,
    },
}

/// Checks that `segment_size` satisfies the format's constraints.
pub fn validate_segment_size(segment_size: u64) -> Result<(), CorruptReason> {
    let _ = segment_size;
    todo!("M2")
}

/// Largest payload, in bytes, that fits in a segment of `segment_size` bytes.
pub fn max_payload_len(segment_size: u64) -> usize {
    let _ = segment_size;
    todo!("M2")
}

/// Rounds up to the next multiple of [`RECORD_ALIGN`].
pub const fn align8(n: usize) -> usize {
    (n + (RECORD_ALIGN - 1)) & !(RECORD_ALIGN - 1)
}

/// Encodes a segment header into `buf[..64]`.
pub fn encode_segment_header(header: &SegmentHeader, buf: &mut [u8; SEGMENT_HEADER_LEN]) {
    let _ = (header, buf);
    todo!("M2")
}

/// Decodes and validates a segment header (magic, version, header length, flags, CRC, size constraints).
///
/// Comparing with the actual file size and the file name is the caller's job.
pub fn decode_segment_header(buf: &[u8]) -> Result<SegmentHeader, CorruptReason> {
    let _ = buf;
    todo!("M2")
}

/// Writes a record's bytes `[4, len)` (CRC, header fields, payload) into `buf`, which starts at the record's
/// position, and returns the commit word (`len`) that the caller must then publish with a release store at offset 0.
///
/// `buf` must be at least `RECORD_HEADER_LEN + payload.len()` bytes long; the caller checks that the record fits.
pub fn encode_record(header: &RecordHeader, payload: &[u8], buf: &mut [u8]) -> u32 {
    let _ = (header, payload, buf);
    todo!("M2")
}

/// Writes the seal's checksum into `buf[4..8]` and returns the commit word ([`SEAL_MARKER`]) to publish at offset 0.
pub fn encode_seal(next_seq: u64, buf: &mut [u8; SEAL_LEN]) -> u32 {
    let _ = (next_seq, buf);
    todo!("M2")
}

/// Decodes the slot at `offset` in a segment image `segment`, given its commit word (already loaded, atomically,
/// by the caller) and the sequence number expected there. Validates it fully (`docs/format.md` → Record).
pub fn decode_at(
    segment: &[u8],
    offset: usize,
    commit_word: u32,
    expected_seq: u64,
) -> Result<Slot<'_>, CorruptReason> {
    let _ = (segment, offset, commit_word, expected_seq);
    todo!("M2")
}

/// Where a forward scan of a segment image stopped.
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
    pub end_offset: usize,
    /// Sequence number the next record would have.
    pub next_seq: u64,
    /// What is at `end_offset`.
    pub end: ScanEnd,
    /// Offset one past the last non-zero byte in the data area at or after `end_offset`, or `end_offset` if there is
    /// none. Recovery zeroes (or quarantines) `[end_offset, dirty_end)`.
    pub dirty_end: usize,
}

/// Scans a whole in-memory segment image from the first record, validating each record, and reports where the
/// valid prefix ends. Used by writer recovery, `verify`, and the fuzzers. Reads commit words with ordinary loads,
/// so it must only run on images that no writer is modifying.
pub fn scan_segment(segment: &[u8], header: &SegmentHeader) -> ScanResult {
    let _ = (segment, header);
    todo!("M2")
}

/// The file name of the segment whose first record is `base_seq`, e.g. `00000000000000000042.tape`.
pub fn segment_file_name(base_seq: u64) -> String {
    format!("{base_seq:020}.{SEGMENT_EXTENSION}")
}

/// Parses a segment file name back into its `base_seq`. Returns `None` for any other name.
pub fn parse_segment_file_name(name: &str) -> Option<u64> {
    let _ = name;
    todo!("M2")
}
