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
    if !(SEGMENT_SIZE_UNIT..=MAX_SEGMENT_SIZE).contains(&segment_size)
        || segment_size % SEGMENT_SIZE_UNIT != 0
    {
        return Err(CorruptReason::BadSegmentHeader("invalid segment size"));
    }
    Ok(())
}

/// Largest payload, in bytes, that fits in a segment of `segment_size` bytes:
/// `min(segment_size - 112, MAX_FRAME_LEN - 40)`. `segment_size` must be valid.
pub fn max_payload_len(segment_size: u64) -> u64 {
    segment_size
        .saturating_sub(112)
        .min(u64::from(MAX_FRAME_LEN) - 40)
}

/// Rounds up to the next multiple of [`RECORD_ALIGN`].
/// Returns `u64::MAX` if the rounded value cannot be represented (never occurs for a frame length).
pub const fn align8(n: u64) -> u64 {
    match n.checked_add(RECORD_ALIGN as u64 - 1) {
        Some(rounded) => rounded & !(RECORD_ALIGN as u64 - 1),
        None => u64::MAX,
    }
}

/// True if a record of frame length `len` starting at `offset` fits in the record area of a segment of
/// `segment_size` bytes (`offset + align8(len) <= segment_size - 8`), computed without overflow.
pub fn record_fits(segment_size: u64, offset: u64, len: u32) -> bool {
    let Some(limit) = segment_size.checked_sub(SEGMENT_TRAILER_LEN as u64) else {
        return false;
    };
    offset
        .checked_add(align8(u64::from(len)))
        .is_some_and(|end| end <= limit)
}

/// Encodes a segment header. `header.segment_size` must pass [`validate_segment_size`].
pub fn encode_segment_header(header: &SegmentHeader) -> [u8; SEGMENT_HEADER_LEN] {
    debug_assert!(validate_segment_size(header.segment_size).is_ok());
    let mut buf = [0; SEGMENT_HEADER_LEN];
    put(&mut buf, 0, &MAGIC);
    put(&mut buf, 8, &FORMAT_VERSION.to_le_bytes());
    put(&mut buf, 10, &(SEGMENT_HEADER_LEN as u16).to_le_bytes());
    put(&mut buf, 16, &header.segment_size.to_le_bytes());
    put(&mut buf, 24, &header.base_seq.to_le_bytes());
    put(&mut buf, 32, &header.created_unix_ns.to_le_bytes());
    let crc = crc32c::crc32c(&buf[..60]);
    put(&mut buf, 60, &crc.to_le_bytes());
    buf
}

/// Decodes and validates a segment header (magic, version, header length, flags, CRC, size constraints).
///
/// Comparing with the actual file size and the file name is the caller's job.
pub fn decode_segment_header(
    buf: &[u8; SEGMENT_HEADER_LEN],
) -> Result<SegmentHeader, CorruptReason> {
    if buf[..8] != MAGIC {
        return Err(CorruptReason::BadMagic);
    }
    let bad = CorruptReason::BadSegmentHeader;
    if read_u16(buf, 8) != Some(FORMAT_VERSION) {
        return Err(bad("unsupported format version"));
    }
    if read_u16(buf, 10) != Some(SEGMENT_HEADER_LEN as u16) {
        return Err(bad("invalid header length"));
    }
    if read_u32(buf, 12) != Some(0) || buf[40..60].iter().any(|&b| b != 0) {
        return Err(bad("non-zero reserved fields"));
    }
    if read_u32(buf, 60) != Some(crc32c::crc32c(&buf[..60])) {
        return Err(bad("header checksum mismatch"));
    }
    let segment_size = read_u64(buf, 16).ok_or(bad("missing segment size"))?;
    validate_segment_size(segment_size)?;
    Ok(SegmentHeader {
        segment_size,
        base_seq: read_u64(buf, 24).ok_or(bad("missing base sequence"))?,
        created_unix_ns: read_u64(buf, 32).ok_or(bad("missing creation timestamp"))?,
    })
}

/// The frame length (commit word value) for a payload of `payload_len` bytes, or `None` if it exceeds
/// [`MAX_FRAME_LEN`].
pub fn frame_len(payload_len: usize) -> Option<u32> {
    let len = u64::try_from(payload_len)
        .ok()?
        .checked_add(RECORD_HEADER_LEN as u64)?;
    if len > u64::from(MAX_FRAME_LEN) {
        return None;
    }
    u32::try_from(len).ok()
}

/// Writes a record body (`[P+4, P+len)`: CRC, header fields, payload) into `body`, which must be exactly
/// `len - 4` bytes for `len = frame_len(payload.len())`. Returns `len`, which the caller then publishes as the
/// commit word with a release store.
pub fn encode_record_body(header: &RecordHeader, payload: &[u8], body: &mut [u8]) -> u32 {
    let Some(len) = frame_len(payload.len()) else {
        debug_assert!(false, "payload exceeds maximum frame length");
        return 0;
    };
    debug_assert_eq!(body.len() as u64, u64::from(len) - 4);
    write_record_fields(header, body);
    put(body, 36, payload);
    finish_record_body(len, body);
    len
}

/// Fills in the CRC of a body whose header fields and payload are already in place (used after
/// [`write_record_fields`] and an in-place payload fill). `len` must be a valid frame length and
/// `body` must contain exactly `len - 4` bytes (`[P+4, P+len)`).
pub fn finish_record_body(len: u32, body: &mut [u8]) {
    debug_assert!((RECORD_HEADER_LEN as u32..=MAX_FRAME_LEN).contains(&len));
    debug_assert_eq!(body.len() as u64, u64::from(len).saturating_sub(4));
    if let Some(fields) = body.get(4..) {
        let crc = record_crc(len, fields);
        put(body, 0, &crc.to_le_bytes());
    }
}

/// Writes the header fields (`seq` through `local_ts_ns`, reserved flags zero) into `body[4..36]`, leaving the CRC
/// and payload alone. `body` must contain at least 36 bytes.
pub fn write_record_fields(header: &RecordHeader, body: &mut [u8]) {
    debug_assert!(body.len() >= RECORD_HEADER_LEN - COMMIT_WORD_LEN);
    put(body, 4, &header.seq.to_le_bytes());
    put(body, 12, &header.schema_id.to_le_bytes());
    put(body, 16, &0_u32.to_le_bytes());
    put(body, 20, &header.venue_ts_ns.to_le_bytes());
    put(body, 28, &header.local_ts_ns.to_le_bytes());
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
    let bad_len = CorruptReason::BadLength(commit_word);
    if !(RECORD_HEADER_LEN as u32..=MAX_FRAME_LEN).contains(&commit_word)
        || body.len() as u64 != u64::from(commit_word) - COMMIT_WORD_LEN as u64
    {
        return Err(bad_len);
    }
    // The exact body length above proves all fixed field ranges below exist.
    if read_u32(body, 0) != Some(record_crc(commit_word, &body[4..])) {
        return Err(CorruptReason::BadChecksum);
    }
    let seq = read_u64(body, 4).ok_or(bad_len)?;
    if seq != expected_seq {
        return Err(CorruptReason::BadSequence {
            expected: expected_seq,
            found: seq,
        });
    }
    if read_u32(body, 16) != Some(0) {
        return Err(CorruptReason::BadFlags);
    }
    Ok((
        RecordHeader {
            seq,
            schema_id: read_u32(body, 12).ok_or(bad_len)?,
            venue_ts_ns: read_u64(body, 20).ok_or(bad_len)?,
            local_ts_ns: read_u64(body, 28).ok_or(bad_len)?,
        },
        &body[36..],
    ))
}

/// The `seal_crc` value for a seal announcing `next_seq`.
pub fn seal_crc(next_seq: u64) -> u32 {
    crc32c::crc32c_append(
        crc32c::crc32c(&SEAL_MARKER.to_le_bytes()),
        &next_seq.to_le_bytes(),
    )
}

/// Encodes one checkpoint marker slot.
pub fn encode_checkpoint_slot(slot: &CheckpointSlot) -> [u8; CHECKPOINT_SLOT_LEN] {
    let mut buf = [0; CHECKPOINT_SLOT_LEN];
    put(&mut buf, 0, &CHECKPOINT_MAGIC);
    put(&mut buf, 8, &FORMAT_VERSION.to_le_bytes());
    put(&mut buf, 16, &slot.generation.to_le_bytes());
    put(&mut buf, 24, &slot.next_seq.to_le_bytes());
    put(&mut buf, 32, &slot.segment_base_seq.to_le_bytes());
    put(&mut buf, 40, &slot.offset.to_le_bytes());
    let crc = crc32c::crc32c(&buf[..60]);
    put(&mut buf, 60, &crc.to_le_bytes());
    buf
}

/// Picks the current checkpoint from the marker file's contents: the valid slot with the higher generation, or
/// `None` if neither is valid. Each slot is considered independently if its complete 64 bytes are present;
/// a truncated file may still contain one valid slot.
pub fn decode_checkpoint(file: &[u8]) -> Option<CheckpointSlot> {
    CHECKPOINT_SLOT_OFFSETS
        .into_iter()
        .filter_map(|offset| {
            let end = (offset as u64).checked_add(CHECKPOINT_SLOT_LEN as u64)?;
            let buf = file.get(offset..usize::try_from(end).ok()?)?;
            if buf[..8] != CHECKPOINT_MAGIC
                || read_u16(buf, 8) != Some(FORMAT_VERSION)
                || buf[10..16].iter().chain(&buf[48..60]).any(|&b| b != 0)
                || read_u32(buf, 60) != Some(crc32c::crc32c(&buf[..60]))
            {
                return None;
            }
            Some(CheckpointSlot {
                generation: read_u64(buf, 16)?,
                next_seq: read_u64(buf, 24)?,
                segment_base_seq: read_u64(buf, 32)?,
                offset: read_u64(buf, 40)?,
            })
        })
        .max_by_key(|slot| slot.generation)
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
        if offset % RECORD_ALIGN as u64 != 0 {
            return None;
        }
        self.with_bytes(offset, COMMIT_WORD_LEN as u64, |bytes| read_u32(bytes, 0))?
    }

    fn with_bytes<R>(&self, offset: u64, len: u64, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
        let end = offset.checked_add(len)?;
        let bytes = self.get(usize::try_from(offset).ok()?..usize::try_from(end).ok()?)?;
        Some(f(bytes))
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
    if offset < SEGMENT_HEADER_LEN as u64 || offset % RECORD_ALIGN as u64 != 0 {
        return Err(CorruptReason::BadOffset(offset));
    }
    let len = segment
        .commit_word(offset)
        .ok_or(CorruptReason::BadOffset(offset))?;
    if len == 0 {
        return Ok(Slot::Empty);
    }
    let body_offset = offset
        .checked_add(COMMIT_WORD_LEN as u64)
        .ok_or(CorruptReason::BadOffset(offset))?;
    if len == SEAL_MARKER {
        if offset
            .checked_add(SEAL_LEN as u64)
            .is_none_or(|end| end > segment.len())
        {
            return Err(CorruptReason::BadSeal);
        }
        let crc = segment
            .with_bytes(body_offset, 4, |bytes| read_u32(bytes, 0))
            .flatten();
        return if crc == Some(seal_crc(expected_seq)) {
            Ok(Slot::Seal)
        } else {
            Err(CorruptReason::BadSeal)
        };
    }
    if !(RECORD_HEADER_LEN as u32..=MAX_FRAME_LEN).contains(&len)
        || !record_fits(segment.len(), offset, len)
    {
        return Err(CorruptReason::BadLength(len));
    }
    let header = segment
        .with_bytes(body_offset, u64::from(len) - 4, |body| {
            decode_record_body(len, body, expected_seq).map(|(header, _)| header)
        })
        .ok_or(CorruptReason::BadLength(len))??;
    let next_offset = offset
        .checked_add(align8(u64::from(len)))
        .ok_or(CorruptReason::BadLength(len))?;
    Ok(Slot::Record {
        header,
        len,
        next_offset,
    })
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
/// A record with sequence `u64::MAX` stops the scan as invalid because it has no representable successor.
pub fn scan_segment<V: SegmentView + ?Sized>(
    segment: &V,
    start_offset: u64,
    start_seq: u64,
) -> ScanResult {
    let mut result = ScanResult {
        end_offset: start_offset,
        next_seq: start_seq,
        records: 0,
        end: ScanEnd::Empty,
    };
    loop {
        result.end = match read_slot(segment, result.end_offset, result.next_seq) {
            Ok(Slot::Empty) => ScanEnd::Empty,
            Ok(Slot::Seal) => ScanEnd::Seal,
            Err(reason) => ScanEnd::Invalid(reason),
            Ok(Slot::Record { next_offset, .. }) => {
                if next_offset <= result.end_offset {
                    break ScanResult {
                        end: ScanEnd::Invalid(CorruptReason::BadOffset(next_offset)),
                        ..result
                    };
                }
                let Some(next_seq) = result.next_seq.checked_add(1) else {
                    break ScanResult {
                        end: ScanEnd::Invalid(CorruptReason::SequenceExhausted),
                        ..result
                    };
                };
                let Some(records) = result.records.checked_add(1) else {
                    break ScanResult {
                        end: ScanEnd::Invalid(CorruptReason::SequenceExhausted),
                        ..result
                    };
                };
                result.end_offset = next_offset;
                result.next_seq = next_seq;
                result.records = records;
                continue;
            }
        };
        return result;
    }
}

/// The file name of the segment whose first record is `base_seq`, e.g. `00000000000000000042.tape`.
pub fn segment_file_name(base_seq: u64) -> String {
    format!("{base_seq:020}.{SEGMENT_EXTENSION}")
}

/// Parses a segment file name back into its `base_seq`. Returns `None` for any other name, including temporary
/// files.
pub fn parse_segment_file_name(name: &str) -> Option<u64> {
    let bytes = name.as_bytes();
    if bytes.len() != 25 || bytes.get(20..) != Some(b".tape") {
        return None;
    }
    bytes[..20].iter().try_fold(0_u64, |value, &digit| {
        if !digit.is_ascii_digit() {
            return None;
        }
        value.checked_mul(10)?.checked_add(u64::from(digit - b'0'))
    })
}

// All offsets and length arithmetic stays in u64 until a checked conversion at the slice boundary.
fn bytes_at<const N: usize>(buf: &[u8], offset: u64) -> Option<[u8; N]> {
    let end = offset.checked_add(N as u64)?;
    buf.get(usize::try_from(offset).ok()?..usize::try_from(end).ok()?)?
        .try_into()
        .ok()
}

fn read_u16(buf: &[u8], offset: u64) -> Option<u16> {
    Some(u16::from_le_bytes(bytes_at(buf, offset)?))
}

fn read_u32(buf: &[u8], offset: u64) -> Option<u32> {
    Some(u32::from_le_bytes(bytes_at(buf, offset)?))
}

fn read_u64(buf: &[u8], offset: u64) -> Option<u64> {
    Some(u64::from_le_bytes(bytes_at(buf, offset)?))
}

fn put(buf: &mut [u8], offset: u64, bytes: &[u8]) {
    if let Some(end) = offset.checked_add(bytes.len() as u64) {
        if let (Ok(start), Ok(end)) = (usize::try_from(offset), usize::try_from(end)) {
            if let Some(dest) = buf.get_mut(start..end) {
                dest.copy_from_slice(bytes);
            }
        }
    }
}

fn record_crc(len: u32, fields_and_payload: &[u8]) -> u32 {
    crc32c::crc32c_append(crc32c::crc32c(&len.to_le_bytes()), fields_and_payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn header() -> SegmentHeader {
        SegmentHeader {
            segment_size: SEGMENT_SIZE_UNIT,
            base_seq: 7,
            created_unix_ns: 123,
        }
    }

    fn record(seq: u64) -> RecordHeader {
        RecordHeader {
            seq,
            schema_id: 9,
            venue_ts_ns: 123,
            local_ts_ns: 456,
        }
    }

    fn checkpoint(generation: u64) -> CheckpointSlot {
        CheckpointSlot {
            generation,
            next_seq: 10,
            segment_base_seq: 7,
            offset: 200,
        }
    }

    fn body(header: &RecordHeader, payload: &[u8]) -> (u32, Vec<u8>) {
        let len = frame_len(payload.len()).unwrap();
        let mut bytes = vec![0; len as usize - 4];
        assert_eq!(encode_record_body(header, payload, &mut bytes), len);
        (len, bytes)
    }

    fn append(image: &mut [u8], offset: usize, header: &RecordHeader, payload: &[u8]) -> usize {
        let (len, body) = body(header, payload);
        image[offset..offset + 4].copy_from_slice(&len.to_le_bytes());
        image[offset + 4..offset + len as usize].copy_from_slice(&body);
        offset + align8(u64::from(len)) as usize
    }

    fn seal(image: &mut [u8], offset: usize, seq: u64) {
        image[offset..offset + 4].copy_from_slice(&SEAL_MARKER.to_le_bytes());
        image[offset + 4..offset + 8].copy_from_slice(&seal_crc(seq).to_le_bytes());
    }

    fn header_crc(bytes: &mut [u8; 64]) {
        let crc = crc32c::crc32c(&bytes[..60]);
        bytes[60..].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn castagnoli_check_value() {
        assert_eq!(crc32c::crc32c(b"123456789"), 0xE306_9283);
        let mut bytes = [0xff; 12];
        bytes[4..].copy_from_slice(&17_u64.to_le_bytes());
        assert_eq!(seal_crc(17), crc32c::crc32c(&bytes));
    }

    #[test]
    fn size_and_length_boundaries() {
        for size in [
            0,
            1,
            65_535,
            65_537,
            MAX_SEGMENT_SIZE + SEGMENT_SIZE_UNIT,
            u64::MAX,
        ] {
            assert!(matches!(
                validate_segment_size(size),
                Err(CorruptReason::BadSegmentHeader(_))
            ));
        }
        for size in [SEGMENT_SIZE_UNIT, 2 * SEGMENT_SIZE_UNIT, MAX_SEGMENT_SIZE] {
            assert_eq!(validate_segment_size(size), Ok(()));
        }
        assert_eq!(max_payload_len(65_536), 65_424);
        assert_eq!(
            max_payload_len(MAX_SEGMENT_SIZE),
            u64::from(MAX_FRAME_LEN) - 40
        );
        assert_eq!(frame_len(0), Some(40));
        assert_eq!(frame_len(MAX_FRAME_LEN as usize - 40), Some(MAX_FRAME_LEN));
        assert_eq!(frame_len(MAX_FRAME_LEN as usize - 39), None);
        assert_eq!(frame_len(usize::MAX), None);
        assert_eq!(align8(0), 0);
        assert_eq!(align8(41), 48);
        assert_eq!(align8(u64::MAX - 7), u64::MAX - 7);
        assert_eq!(align8(u64::MAX), u64::MAX);
        assert!(record_fits(65_536, 64, 65_464));
        assert!(!record_fits(65_536, 64, 65_465));
        assert!(!record_fits(7, 0, 40));
        assert!(!record_fits(u64::MAX, u64::MAX - 7, 40));
    }

    #[test]
    fn header_validation_rules() {
        let good = encode_segment_header(&header());
        assert_eq!(&good[..8], b"TICKTAPE");
        assert_eq!(&good[8..12], &[1, 0, 64, 0]);
        assert_eq!(decode_segment_header(&good), Ok(header()));
        let mut bad = good;
        bad[0] ^= 1;
        assert_eq!(decode_segment_header(&bad), Err(CorruptReason::BadMagic));
        for (offset, value) in [(8, 2), (10, 63), (12, 1), (40, 1), (59, 1)] {
            let mut bad = good;
            bad[offset] = value;
            header_crc(&mut bad);
            assert!(matches!(
                decode_segment_header(&bad),
                Err(CorruptReason::BadSegmentHeader(_))
            ));
        }
        let mut bad = good;
        bad[60] ^= 1;
        assert!(matches!(
            decode_segment_header(&bad),
            Err(CorruptReason::BadSegmentHeader(_))
        ));
        let mut bad = good;
        bad[16..24].copy_from_slice(&65_535_u64.to_le_bytes());
        header_crc(&mut bad);
        assert!(matches!(
            decode_segment_header(&bad),
            Err(CorruptReason::BadSegmentHeader(_))
        ));
    }

    #[test]
    fn body_validation_rules() {
        let (len, good) = body(&record(7), b"abc");
        let (decoded, payload) = decode_record_body(len, &good, 7).unwrap();
        assert_eq!(decoded, record(7));
        assert_eq!(payload, b"abc");
        let mut full = len.to_le_bytes().to_vec();
        full.extend_from_slice(&good[4..]);
        assert_eq!(&good[..4], &crc32c::crc32c(&full).to_le_bytes());
        for len in [0, 1, 39, SEAL_MARKER] {
            assert_eq!(
                decode_record_body(len, &good, 7),
                Err(CorruptReason::BadLength(len))
            );
        }
        for bytes in [&good[..0], &good[..35], &good[..good.len() - 1]] {
            assert_eq!(
                decode_record_body(len, bytes, 7),
                Err(CorruptReason::BadLength(len))
            );
        }
        let mut long = good.clone();
        long.push(0);
        assert_eq!(
            decode_record_body(len, &long, 7),
            Err(CorruptReason::BadLength(len))
        );
        let mut bad = good.clone();
        bad[0] ^= 1;
        assert_eq!(
            decode_record_body(len, &bad, 7),
            Err(CorruptReason::BadChecksum)
        );
        assert_eq!(
            decode_record_body(len, &good, 8),
            Err(CorruptReason::BadSequence {
                expected: 8,
                found: 7
            })
        );
        let mut bad = good;
        bad[16] = 1;
        finish_record_body(len, &mut bad);
        assert_eq!(
            decode_record_body(len, &bad, 7),
            Err(CorruptReason::BadFlags)
        );
    }

    #[test]
    fn in_place_fields_preserve_crc_and_payload() {
        let mut bytes = [0xab; 41];
        write_record_fields(&record(1), &mut bytes);
        assert_eq!(&bytes[..4], &[0xab; 4]);
        assert_eq!(&bytes[36..], &[0xab; 5]);
        finish_record_body(45, &mut bytes);
        assert_eq!(
            decode_record_body(45, &bytes, 1),
            Ok((record(1), &[0xab; 5][..]))
        );
    }

    #[test]
    fn slice_view_checks_every_range() {
        let bytes: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
        assert_eq!(SegmentView::len(bytes), 11);
        assert!(!SegmentView::is_empty(bytes));
        assert!(SegmentView::is_empty(&[][..]));
        assert_eq!(bytes.commit_word(0), Some(0x0403_0201));
        assert_eq!([0_u8; 12][..].commit_word(8), Some(0));
        for offset in [1, 4, 7, 8, 16, u64::MAX - 7, u64::MAX] {
            assert_eq!(bytes.commit_word(offset), None);
        }
        assert_eq!(bytes.with_bytes(1, 3, |b| b.to_vec()), Some(vec![2, 3, 4]));
        assert_eq!(bytes.with_bytes(11, 0, <[u8]>::len), Some(0));
        for (offset, len) in [(12, 0), (10, 2), (0, u64::MAX), (u64::MAX, 2)] {
            assert_eq!(bytes.with_bytes(offset, len, |_| ()), None);
        }
    }

    #[test]
    fn slot_bounds_and_corruption() {
        let mut image = vec![0; 65_536];
        assert_eq!(read_slot(&image[..], 64, 7), Ok(Slot::Empty));
        for offset in [0, 56, 65, 65_536, u64::MAX] {
            assert_eq!(
                read_slot(&image[..], offset, 7),
                Err(CorruptReason::BadOffset(offset))
            );
        }
        image[64..68].copy_from_slice(&39_u32.to_le_bytes());
        assert_eq!(
            read_slot(&image[..], 64, 7),
            Err(CorruptReason::BadLength(39))
        );
        image[64..68].copy_from_slice(&65_465_u32.to_le_bytes());
        assert_eq!(
            read_slot(&image[..], 64, 7),
            Err(CorruptReason::BadLength(65_465))
        );
        append(&mut image, 64, &record(7), b"abc");
        assert_eq!(
            read_slot(&image[..], 64, 7),
            Ok(Slot::Record {
                header: record(7),
                len: 43,
                next_offset: 112
            })
        );
        assert_eq!(
            read_slot(&image[..], 64, 8),
            Err(CorruptReason::BadSequence {
                expected: 8,
                found: 7
            })
        );
        image[68] ^= 1;
        assert_eq!(
            read_slot(&image[..], 64, 7),
            Err(CorruptReason::BadChecksum)
        );
        seal(&mut image, 64, 7);
        assert_eq!(read_slot(&image[..], 64, 7), Ok(Slot::Seal));
        assert_eq!(read_slot(&image[..], 64, 8), Err(CorruptReason::BadSeal));
        assert_eq!(read_slot(&image[..68], 64, 7), Err(CorruptReason::BadSeal));
    }

    #[test]
    fn exact_capacity_and_trailer_seal() {
        let mut image = vec![0; 65_536];
        let end = append(&mut image, 64, &record(7), &vec![3; 65_424]);
        assert_eq!(end, 65_528);
        seal(&mut image, end, 8);
        assert_eq!(read_slot(&image[..], end as u64, 8), Ok(Slot::Seal));
        assert_eq!(
            scan_segment(&image[..], 64, 7),
            ScanResult {
                end_offset: 65_528,
                next_seq: 8,
                records: 1,
                end: ScanEnd::Seal
            }
        );
    }

    #[test]
    fn scan_stops_at_first_invalid_and_at_empty() {
        let mut image = vec![0; 65_536];
        let end = append(&mut image, 64, &record(7), b"a");
        assert_eq!(
            scan_segment(&image[..], 64, 7),
            ScanResult {
                end_offset: end as u64,
                next_seq: 8,
                records: 1,
                end: ScanEnd::Empty
            }
        );
        append(&mut image, end, &record(9), b"b");
        assert_eq!(
            scan_segment(&image[..], 64, 7).end,
            ScanEnd::Invalid(CorruptReason::BadSequence {
                expected: 8,
                found: 9
            })
        );
        assert_eq!(scan_segment(&image[..], u64::MAX, 7).end_offset, u64::MAX);
        append(&mut image, 64, &record(u64::MAX - 1), b"");
        assert_eq!(
            scan_segment(&image[..], 64, u64::MAX - 1).next_seq,
            u64::MAX
        );
    }

    #[test]
    fn scan_sequence_exhaustion_is_invalid() {
        let mut image = vec![0; 128];
        append(&mut image, 64, &record(u64::MAX), b"");
        assert_eq!(
            scan_segment(&image[..], 64, u64::MAX),
            ScanResult {
                end_offset: 64,
                next_seq: u64::MAX,
                records: 0,
                end: ScanEnd::Invalid(CorruptReason::SequenceExhausted),
            }
        );
    }

    #[test]
    fn every_committed_byte_is_protected_but_padding_is_not_checksummed() {
        let mut image = vec![0; 256];
        let end = append(&mut image, 64, &record(7), b"abc");
        for position in 64..107 {
            image[position] ^= 1;
            if image[..].commit_word(64) != Some(0) {
                assert!(
                    matches!(scan_segment(&image[..], 64, 7).end, ScanEnd::Invalid(_)),
                    "byte {position}"
                );
            }
            image[position] ^= 1;
        }
        image[107..end].fill(0xff);
        assert_eq!(scan_segment(&image[..], 64, 7).end, ScanEnd::Empty);
    }

    #[test]
    fn checkpoint_selection_and_validation() {
        let first = encode_checkpoint_slot(&checkpoint(1));
        let second = encode_checkpoint_slot(&checkpoint(2));
        let mut file = vec![0; CHECKPOINT_FILE_LEN];
        assert_eq!(decode_checkpoint(&file), None);
        assert_eq!(decode_checkpoint(&file[..63]), None);
        file[..64].copy_from_slice(&first);
        assert_eq!(decode_checkpoint(&file), Some(checkpoint(1)));
        assert_eq!(decode_checkpoint(&file[..64]), Some(checkpoint(1)));
        file[65_536..65_600].copy_from_slice(&second);
        assert_eq!(decode_checkpoint(&file), Some(checkpoint(2)));
        file[65_536 + 60] ^= 1;
        assert_eq!(decode_checkpoint(&file), Some(checkpoint(1)));
        file[65_536 + 60] ^= 1;
        file[60] ^= 1;
        assert_eq!(decode_checkpoint(&file), Some(checkpoint(2)));
        file[65_536 + 60] ^= 1;
        assert_eq!(decode_checkpoint(&file), None);
        for offset in [0, 8, 10, 15, 48, 59, 60] {
            let mut bad = first;
            bad[offset] ^= 1;
            if offset != 60 {
                header_crc(&mut bad);
            }
            assert_eq!(decode_checkpoint(&bad), None, "offset {offset}");
        }
        file[..64].copy_from_slice(&second);
        file[65_536..65_600].copy_from_slice(&first);
        assert_eq!(decode_checkpoint(&file), Some(checkpoint(2)));
    }

    #[test]
    fn file_names_are_exact_ascii_decimal() {
        for seq in [0, 42, u64::MAX] {
            assert_eq!(parse_segment_file_name(&segment_file_name(seq)), Some(seq));
        }
        for name in [
            "",
            "42.tape",
            "000000000000000000042.tape",
            "00000000000000000042.tape.tmp",
            "00000000000000000042.TAPE",
            "0000000000000000004 .tape",
            "+0000000000000000042.tape",
            "-0000000000000000042.tape",
            "18446744073709551616.tape",
            "０００００００００００００００００００４２.tape",
            "00000000000000000042.tape\n",
        ] {
            assert_eq!(parse_segment_file_name(name), None, "{name}");
        }
    }

    fn property_config() -> ProptestConfig {
        let mut config = ProptestConfig::with_cases(64);
        if cfg!(miri) {
            // Keep the interpreter run bounded and avoid filesystem-backed failure persistence:
            // Miri's default isolation does not support proptest's getcwd call.
            config.cases = 2;
            config.failure_persistence = None;
        }
        config
    }

    proptest! {
        #![proptest_config(property_config())]

        #[test]
        fn header_round_trip(units in 1_u64..=(MAX_SEGMENT_SIZE / SEGMENT_SIZE_UNIT), base_seq in any::<u64>(), created_unix_ns in any::<u64>()) {
            let header = SegmentHeader { segment_size: units * SEGMENT_SIZE_UNIT, base_seq, created_unix_ns };
            prop_assert_eq!(decode_segment_header(&encode_segment_header(&header)), Ok(header));
        }

        #[test]
        fn record_round_trip(seq in any::<u64>(), schema_id in any::<u32>(), venue_ts_ns in any::<u64>(), local_ts_ns in any::<u64>(), payload in prop::collection::vec(any::<u8>(), 0..2048)) {
            let header = RecordHeader { seq, schema_id, venue_ts_ns, local_ts_ns };
            let (len, body) = body(&header, &payload);
            prop_assert_eq!(decode_record_body(len, &body, seq), Ok((header, &payload[..])));
        }

        #[test]
        fn checkpoint_round_trip(generation in any::<u64>(), next_seq in any::<u64>(), segment_base_seq in any::<u64>(), offset in any::<u64>()) {
            let slot = CheckpointSlot { generation, next_seq, segment_base_seq, offset };
            prop_assert_eq!(decode_checkpoint(&encode_checkpoint_slot(&slot)), Some(slot));
        }

        #[test]
        fn segment_round_trip_and_byte_corruption(
            records in prop::collection::vec((any::<u32>(), any::<u64>(), any::<u64>(), prop::collection::vec(any::<u8>(), 0..256)), 0..12),
            base in 0_u64..(u64::MAX - 12), sealed in any::<bool>(), selection in any::<usize>(), byte in any::<usize>(), xor in 1_u8..=255,
        ) {
            let mut image = vec![0; 65_536];
            image[..64].copy_from_slice(&encode_segment_header(&SegmentHeader { base_seq: base, ..header() }));
            let mut offset = 64;
            let mut positions = Vec::new();
            for (i, (schema_id, venue_ts_ns, local_ts_ns, payload)) in records.iter().enumerate() {
                let header = RecordHeader { seq: base + i as u64, schema_id: *schema_id, venue_ts_ns: *venue_ts_ns, local_ts_ns: *local_ts_ns };
                positions.push((offset, 40 + payload.len()));
                let next = append(&mut image, offset, &header, payload);
                prop_assert_eq!(read_slot(&image[..], offset as u64, header.seq), Ok(Slot::Record { header, len: (40 + payload.len()) as u32, next_offset: next as u64 }));
                offset = next;
            }
            let next_seq = base + records.len() as u64;
            if sealed { seal(&mut image, offset, next_seq); }
            prop_assert_eq!(scan_segment(&image[..], 64, base), ScanResult { end_offset: offset as u64, next_seq, records: records.len() as u64, end: if sealed { ScanEnd::Seal } else { ScanEnd::Empty } });
            if !positions.is_empty() {
                let selected = selection % positions.len();
                let (start, len) = positions[selected];
                image[start + byte % len] ^= xor;
                // A commit word corrupted to zero is explicitly the clean-end sentinel.
                if image[..].commit_word(start as u64) != Some(0) {
                    let scan = scan_segment(&image[..], 64, base);
                    prop_assert!(matches!(scan.end, ScanEnd::Invalid(_)));
                    prop_assert!(scan.end_offset <= start as u64);
                    prop_assert!(scan.records <= selected as u64);
                }
            }
        }
    }
}
