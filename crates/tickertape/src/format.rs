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
    let crc = crate::crc::crc32c(&buf[..60]);
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
    if read_u32(buf, 60) != Some(crate::crc::crc32c(&buf[..60])) {
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
    Ok((
        decode_record_fields(commit_word, body, expected_seq)?,
        &body[36..],
    ))
}

/// The header fields of a record body whose length and CRC32C are already checked: `body` holds at least its first
/// 36 bytes.
fn decode_record_fields(
    commit_word: u32,
    body: &[u8],
    expected_seq: u64,
) -> Result<RecordHeader, CorruptReason> {
    let bad_len = CorruptReason::BadLength(commit_word);
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
    Ok(RecordHeader {
        seq,
        schema_id: read_u32(body, 12).ok_or(bad_len)?,
        venue_ts_ns: read_u64(body, 20).ok_or(bad_len)?,
        local_ts_ns: read_u64(body, 28).ok_or(bad_len)?,
    })
}

/// Validates and decodes a record read word by word from shared memory: the zero-copy reader's version of
/// [`decode_record_body`] (ADR 0027). It makes the same checks in the same order and returns the same results for
/// the same bytes, without any slice of the body.
///
/// `commit_word` is the value loaded from `P` (not a seal, not zero); `crc` is the `u32` at `P+4`; `words` are the
/// 8-byte words covering `[P+8, P+align8(len))`: four header words, then the payload, whose last word may end in
/// padding (never checked, as in [`decode_record_body`], which doesn't see it). Each word is loaded exactly once,
/// so the header returned is the one the CRC covered. A word count that doesn't match `commit_word` is
/// [`CorruptReason::BadLength`], like a body of the wrong length. Bounds against the segment are the caller's job.
pub(crate) fn decode_record_words(
    commit_word: u32,
    crc: u32,
    words: crate::mmap::SharedWords<'_>,
    expected_seq: u64,
) -> Result<RecordHeader, CorruptReason> {
    let bad_len = CorruptReason::BadLength(commit_word);
    if !(RECORD_HEADER_LEN as u32..=MAX_FRAME_LEN).contains(&commit_word) {
        return Err(bad_len);
    }
    let len = u64::from(commit_word);
    // Header words after the commit word and CRC: seq, schema id and flags, venue and local timestamps.
    const FIELD_WORDS: usize = (RECORD_HEADER_LEN - 8) / 8;
    if words.len() as u64 != (align8(len) - 8) / 8 {
        return Err(bad_len);
    }
    let payload_len = len - RECORD_HEADER_LEN as u64;
    let full_words = usize::try_from(payload_len / 8).map_err(|_| bad_len)?;
    let tail_len = (payload_len % 8) as usize;
    let (fields, payload) = words.split_at_checked(FIELD_WORDS).ok_or(bad_len)?;
    let (full, last) = payload.split_at_checked(full_words).ok_or(bad_len)?;
    let mut field = [0u64; FIELD_WORDS];
    for (dest, word) in field.iter_mut().zip(fields.iter()) {
        *dest = word;
    }
    // The CRC covers the commit word's 4 bytes, the header words, and the payload without its padding.
    let mut head = [0u8; 4 + 8 * FIELD_WORDS];
    put(&mut head, 0, &commit_word.to_le_bytes());
    for (i, word) in field.iter().enumerate() {
        put(&mut head, 4 + 8 * i as u64, &word.to_le_bytes());
    }
    let tail = if tail_len == 0 {
        [0u8; 8]
    } else {
        last.get(0).ok_or(bad_len)?.to_le_bytes()
    };
    let computed = crate::crc::crc32c_append_parts(0, &head, full, &tail[..tail_len]);
    if crc != computed {
        return Err(CorruptReason::BadChecksum);
    }
    let [seq, schema_and_flags, venue_ts_ns, local_ts_ns] = field;
    if seq != expected_seq {
        return Err(CorruptReason::BadSequence {
            expected: expected_seq,
            found: seq,
        });
    }
    if (schema_and_flags >> 32) as u32 != 0 {
        return Err(CorruptReason::BadFlags);
    }
    Ok(RecordHeader {
        seq,
        schema_id: schema_and_flags as u32,
        venue_ts_ns,
        local_ts_ns,
    })
}

/// Fuzzing hook, not part of the API: [`decode_record_words`] over a body (`[P+4, P+len)`) laid out in ordinary
/// memory as the zero-copy reader would see it. The padding up to the next 8-byte boundary is filled with non-zero
/// bytes, because a mapping's padding is only zero while nothing has damaged it, and the decoder must ignore it.
/// `None` unless `body.len() == commit_word - 4`, because the reader always takes the word count from the commit
/// word.
#[cfg(fuzzing)]
#[doc(hidden)]
pub fn decode_record_words_for_fuzzing(
    commit_word: u32,
    body: &[u8],
    expected_seq: u64,
) -> Option<Result<RecordHeader, CorruptReason>> {
    if body.len() as u64 + COMMIT_WORD_LEN as u64 != u64::from(commit_word) || body.len() < 4 {
        return None;
    }
    let crc = read_u32(body, 0)?;
    let words = words_of_padded(&body[4..], 0xa5);
    Some(decode_record_words(
        commit_word,
        crc,
        crate::mmap::SharedWords::new(&words),
        expected_seq,
    ))
}

/// `bytes` as atomic little-endian words, the last one zero-padded.
#[cfg(test)]
fn words_of(bytes: &[u8]) -> Vec<std::sync::atomic::AtomicU64> {
    words_of_padded(bytes, 0)
}

/// `bytes` as atomic little-endian words, the last one padded with `pad`.
#[cfg(any(test, fuzzing))]
fn words_of_padded(bytes: &[u8], pad: u8) -> Vec<std::sync::atomic::AtomicU64> {
    bytes
        .chunks(8)
        .map(|chunk| {
            let mut word = [pad; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            std::sync::atomic::AtomicU64::new(u64::from_le_bytes(word))
        })
        .collect()
}

/// The `seal_crc` value for a seal announcing `next_seq`.
pub fn seal_crc(next_seq: u64) -> u32 {
    crate::crc::crc32c_append(
        crate::crc::crc32c(&SEAL_MARKER.to_le_bytes()),
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
    let crc = crate::crc::crc32c(&buf[..60]);
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
            decode_checkpoint_slot(file.get(offset..usize::try_from(end).ok()?)?)
        })
        // `max_by_key` returns the last of equal maxima: on a tie the slot at 65,536 wins (docs/format.md).
        .max_by_key(|slot| slot.generation)
}

/// Decodes one 64-byte checkpoint slot; `None` if it is not valid (or not 64 bytes long).
pub(crate) fn decode_checkpoint_slot(buf: &[u8]) -> Option<CheckpointSlot> {
    if buf.len() != CHECKPOINT_SLOT_LEN
        || buf[..8] != CHECKPOINT_MAGIC
        || read_u16(buf, 8) != Some(FORMAT_VERSION)
        || buf[10..16].iter().chain(&buf[48..60]).any(|&b| b != 0)
        || read_u32(buf, 60) != Some(crate::crc::crc32c(&buf[..60]))
    {
        return None;
    }
    Some(CheckpointSlot {
        generation: read_u64(buf, 16)?,
        next_seq: read_u64(buf, 24)?,
        segment_base_seq: read_u64(buf, 32)?,
        offset: read_u64(buf, 40)?,
    })
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
    /// The longest range a single [`with_bytes`](Self::with_bytes) call should ask for. A view that copies (the
    /// `pread` view) allocates the range it is asked for, so validation reads a record body longer than this in
    /// pieces instead, before its CRC32C has shown that its length is real. Views over memory have no limit.
    fn max_piece(&self) -> u64 {
        u64::MAX
    }
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
        // An empty segment is never sealed (a seal at the first slot would name its own segment).
        if offset == SEGMENT_HEADER_LEN as u64
            || offset
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
    let body_len = u64::from(len) - COMMIT_WORD_LEN as u64;
    let header = if body_len <= segment.max_piece() {
        segment
            .with_bytes(body_offset, body_len, |body| {
                decode_record_body(len, body, expected_seq).map(|(header, _)| header)
            })
            .ok_or(CorruptReason::BadLength(len))??
    } else {
        decode_record_in_pieces(segment, body_offset, len, expected_seq)?
    };
    let next_offset = offset
        .checked_add(align8(u64::from(len)))
        .ok_or(CorruptReason::BadLength(len))?;
    Ok(Slot::Record {
        header,
        len,
        next_offset,
    })
}

/// [`decode_record_body`] for a body longer than the view's [`SegmentView::max_piece`], already checked to lie inside
/// the segment: the CRC32C is computed over pieces of at most that size (and at most 1 MiB), so a damaged length
/// costs no allocation, and the header fields are decoded only once the CRC matches. Same result, same order of
/// checks. Every byte is read once: the header fields are copied out of the first piece the CRC covers, so the header
/// returned is the one the CRC covered even if the file changes under a `pread` view between pieces.
fn decode_record_in_pieces<V: SegmentView + ?Sized>(
    segment: &V,
    body_offset: u64,
    len: u32,
    expected_seq: u64,
) -> Result<RecordHeader, CorruptReason> {
    const FIELDS: usize = RECORD_HEADER_LEN - 8;
    let bad_len = CorruptReason::BadLength(len);
    // At least one header, so the first piece holds all the fields (the body after the CRC is at least `FIELDS`).
    let piece_len = segment.max_piece().clamp(RECORD_HEADER_LEN as u64, 1 << 20);
    let stored = segment
        .with_bytes(body_offset, 4, |b| read_u32(b, 0))
        .flatten()
        .ok_or(bad_len)?;
    // The body as `decode_record_fields` expects it: the stored CRC, then the fields from the first piece.
    let mut header = [0_u8; RECORD_HEADER_LEN - 4];
    header[..4].copy_from_slice(&stored.to_le_bytes());
    let mut crc = crate::crc::crc32c(&len.to_le_bytes());
    let mut at = body_offset.checked_add(4).ok_or(bad_len)?;
    let mut left = u64::from(len) - 8;
    let mut first = true;
    while left > 0 {
        let piece = left.min(piece_len);
        crc = segment
            .with_bytes(at, piece, |bytes| {
                if first {
                    header[4..].copy_from_slice(bytes.get(..FIELDS)?);
                }
                Some(crate::crc::crc32c_append(crc, bytes))
            })
            .flatten()
            .ok_or(bad_len)?;
        first = false;
        at += piece;
        left -= piece;
    }
    if crc != stored {
        return Err(CorruptReason::BadChecksum);
    }
    decode_record_fields(len, &header, expected_seq)
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

/// Evidence, found by [`find_history_after_empty`], that an empty slot (commit word 0) is not where the committed
/// part of a segment ends (`docs/format.md` → Recovery, ADR 0035).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LaterHistory {
    /// A record at `offset` that validates completely (length, CRC32C, flags) with sequence number `seq`, which is
    /// above the empty slot's expected sequence number by no more than the records the space between them can hold.
    Record {
        /// Where the record starts.
        offset: u64,
        /// Its sequence number.
        seq: u64,
    },
    /// A valid seal at `offset` announcing `next_seq` (in the same range), followed only by zeros.
    Seal {
        /// Where the seal starts.
        offset: u64,
        /// The sequence number it announces.
        next_seq: u64,
    },
    /// So many slots carry headers of this journal's following records that checking them all would exceed the work
    /// bound: every candidate checked before it failed its checksum, and the one that would exceed it was not checked
    /// (its checksum is unknown). `offset` and `seq` are those of the slot that exceeded it (for a final seal, the
    /// expected sequence number plus one). Only header-shaped bytes can do this, from damage or from a torn payload
    /// that holds them (ADR 0035). Inconclusive: nothing was proven valid, so it is reported as
    /// [`CorruptReason::UncheckedTail`], not [`CorruptReason::EmptyCommitWord`].
    Unchecked {
        /// The slot that exceeded the bound.
        offset: u64,
        /// The sequence number in its header.
        seq: u64,
    },
}

impl LaterHistory {
    /// The sequence number this evidence carries.
    pub fn seq(&self) -> u64 {
        match *self {
            LaterHistory::Record { seq, .. } | LaterHistory::Unchecked { seq, .. } => seq,
            LaterHistory::Seal { next_seq, .. } => next_seq,
        }
    }

    /// How recovery and `verify` report this evidence: [`CorruptReason::EmptyCommitWord`] for a record or seal that
    /// validated, [`CorruptReason::UncheckedTail`] when the search stopped at its work bound before deciding.
    pub fn reason(&self) -> CorruptReason {
        match *self {
            LaterHistory::Unchecked { seq, .. } => CorruptReason::UncheckedTail(seq),
            _ => CorruptReason::EmptyCommitWord(self.seq()),
        }
    }
}

/// Chunk size for [`nonzero_end`].
const ZERO_SCAN_CHUNK: u64 = 1 << 20;

/// The end (exclusive) of the last non-zero byte in `[from, len)` of `segment`, or `None` if all of it is zero (or
/// `from` is out of bounds). Reads backwards in 1 MiB chunks, so a windowed view never needs more than one chunk.
pub fn nonzero_end<V: SegmentView + ?Sized>(segment: &V, from: u64) -> Option<u64> {
    let mut end = segment.len();
    while end > from {
        let start = end.saturating_sub(ZERO_SCAN_CHUNK).max(from);
        let last = segment.with_bytes(start, end - start, |chunk| {
            // Eight bytes at a time keeps unoptimised builds fast.
            let words = chunk.len() / 8 * 8;
            let tail = chunk[words..].iter().rposition(|&b| b != 0);
            tail.map(|i| words + i).or_else(|| {
                chunk[..words]
                    .chunks_exact(8)
                    .rposition(|w| w != [0; 8])
                    .and_then(|w| {
                        chunk[w * 8..w * 8 + 8]
                            .iter()
                            .rposition(|&b| b != 0)
                            .map(|i| w * 8 + i)
                    })
            })
        })?;
        if let Some(i) = last {
            return Some(start + i as u64 + 1);
        }
        end = start;
    }
    None
}

/// Looks for committed history after an empty slot at `offset` (commit word 0) where a scan expecting
/// `expected_seq` stopped, within the same segment (ADR 0035).
///
/// A torn append leaves non-zero bytes only inside the one frame being written, from `offset`, so the only way its
/// bytes can contain a valid frame is through a payload that holds one. This returns evidence only for a frame or
/// seal that also has a sequence number its position allows: a record at `Q` with sequence `T` such that
/// `expected_seq < T <= expected_seq + (Q - offset) / 40` (every record is at least 40 bytes), or a seal in the same
/// range, and at most [`SEAL_SEARCH_RECORDS`] after `expected_seq`, that is the last non-zero item in the segment. The first such record is returned.
///
/// Candidates inside a completely written torn frame are not evidence: if the frame at `offset` has an intact header
/// apart from its commit word (sequence number `expected_seq`, flags zero, and a stored CRC that matches for a length
/// covering every non-zero byte after `offset`, allowing up to 63 trailing zero bytes, but not also for a length that
/// ends at the first candidate), its payload holds the candidates and this returns `None`.
///
/// Work: one backward pass from the end of `[offset, len)` to find its last non-zero byte, cheap header checks at each 8-byte position up
/// to it, and CRC32C over at most twice the remaining segment length for candidate records (beyond that it returns
/// [`LaterHistory::Unchecked`]), plus one CRC of 12 bytes for each of at most [`SEAL_SEARCH_RECORDS`] next sequence
/// numbers for a final seal (inside the same bound), and, once,
/// CRC32C over the torn frame at `offset` up to the first candidate and up to the last non-zero byte.
pub fn find_history_after_empty<V: SegmentView + ?Sized>(
    segment: &V,
    offset: u64,
    expected_seq: u64,
) -> Option<LaterHistory> {
    let end = nonzero_end(segment, offset.checked_add(COMMIT_WORD_LEN as u64)?)?;
    find_history_before(segment, offset, expected_seq, end)
}

/// [`find_history_after_empty`] with the end of the non-zero bytes after the empty commit word already known.
pub(crate) fn find_history_before<V: SegmentView + ?Sized>(
    segment: &V,
    offset: u64,
    expected_seq: u64,
    end: u64,
) -> Option<LaterHistory> {
    let len = segment.len();
    // CRC32C bytes: twice the rest of the segment for candidate records, plus the bounded final-seal search.
    let budget = len
        .saturating_sub(offset)
        .saturating_mul(2)
        .saturating_add(SEAL_SEARCH_RECORDS * SEAL_CRC_BYTES);
    let mut spent = 0_u64;
    // The first candidate decides: either the torn frame at `offset` covers every non-zero byte (then no candidate
    // can lie outside it), or it is evidence.
    let evidence = |found: LaterHistory| {
        let first = match found {
            LaterHistory::Record { offset, .. }
            | LaterHistory::Seal { offset, .. }
            | LaterHistory::Unchecked { offset, .. } => offset,
        };
        (!torn_frame_covers_tail(segment, offset, expected_seq, end, first)).then_some(found)
    };
    // A record after the empty slot starts at least one minimal frame after it.
    let mut q = offset.checked_add(RECORD_HEADER_LEN as u64)?;
    while q < end {
        let Some(word) = segment.commit_word(q) else {
            break;
        };
        let max_records = (q - offset) / RECORD_HEADER_LEN as u64;
        let in_range = |seq: u64| seq > expected_seq && seq - expected_seq <= max_records;
        if word == SEAL_MARKER {
            // Only a final seal: nothing non-zero after it. At most `SEAL_SEARCH_RECORDS` next sequence numbers are
            // tried, each a chance match for a torn payload ending in a seal marker and four arbitrary bytes.
            if q.saturating_add(SEAL_LEN as u64) >= end {
                let tries = max_records.min(SEAL_SEARCH_RECORDS);
                spent = spent.saturating_add(tries * SEAL_CRC_BYTES);
                if spent > budget {
                    let seq = expected_seq.checked_add(1)?;
                    return evidence(LaterHistory::Unchecked { offset: q, seq });
                }
                let crc = q
                    .checked_add(COMMIT_WORD_LEN as u64)
                    .and_then(|at| segment.with_bytes(at, 4, |b| read_u32(b, 0)))
                    .flatten();
                let prefix = crate::crc::crc32c(&SEAL_MARKER.to_le_bytes());
                let found = crc.and_then(|crc| {
                    (1..=tries)
                        .map_while(|k| expected_seq.checked_add(k))
                        .find(|next| crate::crc::crc32c_append(prefix, &next.to_le_bytes()) == crc)
                });
                if let Some(next_seq) = found {
                    if read_slot(segment, q, next_seq) == Ok(Slot::Seal) {
                        return evidence(LaterHistory::Seal {
                            offset: q,
                            next_seq,
                        });
                    }
                }
            }
        } else if (RECORD_HEADER_LEN as u32..=MAX_FRAME_LEN).contains(&word)
            && record_fits(len, q, word)
        {
            let fields = q
                .checked_add(8)
                .and_then(|at| segment.with_bytes(at, 16, |b| (read_u64(b, 0), read_u32(b, 12))))
                .and_then(|(seq, flags)| Some((seq?, flags?)));
            if let Some((seq, 0)) = fields.filter(|(seq, _)| in_range(*seq)) {
                spent = spent.saturating_add(u64::from(word));
                if spent > budget {
                    return evidence(LaterHistory::Unchecked { offset: q, seq });
                }
                if matches!(read_slot(segment, q, seq), Ok(Slot::Record { .. })) {
                    return evidence(LaterHistory::Record { offset: q, seq });
                }
            }
        }
        q = q.checked_add(RECORD_ALIGN as u64)?;
    }
    None
}

/// How many next sequence numbers the final-seal search tries after an empty commit word (ADR 0035): a final seal
/// counts as evidence only if it announces one of the `SEAL_SEARCH_RECORDS` sequence numbers after the expected one.
/// It covers a zeroed 4 KiB block of minimal records (102) in front of the seal. A longer damaged run before a seal is
/// still found through the successor segment (evidence from a segment with a higher base), unless the successor was
/// lost too. Each number tried is a `2^-32` chance match for a torn payload that ends, as far as it was written, in a
/// seal marker at an 8-byte-aligned position and four arbitrary bytes, followed only by zeros, so this bounds that
/// false positive by `2^-25`.
pub const SEAL_SEARCH_RECORDS: u64 = 128;

/// CRC32C bytes per sequence number tried by the final-seal search (the marker and the sequence number).
const SEAL_CRC_BYTES: u64 = 12;

/// How many trailing zero bytes a completely written torn frame's payload may end with for
/// [`torn_frame_covers_tail`] to recognise it: the frame lengths tried run from the last non-zero byte to this many
/// bytes past it.
const COVER_SLACK: u64 = 63;

/// True if recovery takes the frame at `offset` (commit word 0) for a completely written torn append whose body
/// holds every non-zero byte up to `end`, so that a candidate at `first_candidate` lies inside its payload and is
/// not counted as evidence (ADR 0035). A heuristic on the CRC, not a proof.
///
/// That is the case when its header is intact apart from the commit word (sequence number `expected_seq`, flags
/// zero) and its stored CRC matches for some frame length `L` that fits with `end - offset <= L <= end - offset +`
/// [`COVER_SLACK`]: a writer killed after storing the CRC and before the commit word leaves such bytes. A body
/// torn in the middle has no CRC yet (or one that doesn't match), and a payload that ends in more than
/// [`COVER_SLACK`] zero bytes needs a length outside the range: both stay evidence. Each length tried can match by
/// chance (`2^-32`), so a frame whose CRC also matches for a length ending where the first candidate starts is not
/// covered: that is a complete record followed by the next one, as a zeroed commit word leaves it.
fn torn_frame_covers_tail<V: SegmentView + ?Sized>(
    segment: &V,
    offset: u64,
    expected_seq: u64,
    end: u64,
    first_candidate: u64,
) -> bool {
    let Some(fields_at) = offset.checked_add(COMMIT_WORD_LEN as u64) else {
        return false;
    };
    let fields = segment.with_bytes(fields_at, 20, |b| {
        Some((read_u32(b, 0)?, read_u64(b, 4)?, read_u32(b, 16)?))
    });
    let Some(Some((stored, seq, flags))) = fields else {
        return false;
    };
    if seq != expected_seq || flags != 0 {
        return false;
    }
    let Some(covered) = end.checked_sub(offset) else {
        return false;
    };
    if !crc_matches_some_len(
        segment,
        offset,
        stored,
        covered,
        covered.saturating_add(COVER_SLACK),
    ) {
        return false;
    }
    let before = first_candidate.saturating_sub(offset);
    !crc_matches_some_len(segment, offset, stored, before.saturating_sub(7), before)
}

/// True if the record CRC `stored` of a frame at `offset` matches for some frame length in `lo..=hi` that is valid
/// and fits in the segment. Reads `[offset + 8, offset + hi)` once, at most 1 MiB at a time: the CRC of the shortest
/// length is computed in one pass, and each longer one by moving the length prefix's CRC past the bytes
/// ([`crate::crc::x8n`]).
fn crc_matches_some_len<V: SegmentView + ?Sized>(
    segment: &V,
    offset: u64,
    stored: u32,
    lo: u64,
    hi: u64,
) -> bool {
    let Some(fields_at) = offset.checked_add(8) else {
        return false;
    };
    let limit = segment
        .len()
        .saturating_sub(SEGMENT_TRAILER_LEN as u64)
        .saturating_sub(offset)
        / RECORD_ALIGN as u64
        * RECORD_ALIGN as u64;
    let lo = lo.max(RECORD_HEADER_LEN as u64);
    let hi = hi.min(u64::from(MAX_FRAME_LEN)).min(limit);
    if lo > hi {
        return false;
    }
    // In bounded pieces: a `pread` view allocates a window as large as the range it is asked for, and this range
    // can be most of a segment (`inspect::verify` reads segments that way).
    const PIECE: u64 = 1 << 20;
    let mut crc = 0;
    let (mut at, mut left) = (fields_at, lo - 8);
    while left > 0 {
        let piece = left.min(PIECE);
        let Some(next) =
            segment.with_bytes(at, piece, |bytes| crate::crc::crc32c_append(crc, bytes))
        else {
            return false;
        };
        crc = next;
        at += piece;
        left -= piece;
    }
    let mut shift = crate::crc::x8n(lo - 8);
    segment
        .with_bytes(fields_at + lo - 8, hi - lo, |more| {
            for (len, next) in (lo..=hi).zip(more.iter().map(Some).chain([None])) {
                // `record_crc(len, fields) == crc32c(len ‖ fields)`.
                let prefix = crate::crc::crc32c(&(len as u32).to_le_bytes());
                if crate::crc::mul_mod(shift, prefix) ^ crc == stored {
                    return true;
                }
                if let Some(&byte) = next {
                    crc = crate::crc::crc32c_append(crc, &[byte]);
                    shift = crate::crc::mul_mod(shift, 1 << 23);
                }
            }
            false
        })
        .unwrap_or(false)
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
    crate::crc::crc32c_append(crate::crc::crc32c(&len.to_le_bytes()), fields_and_payload)
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

    /// Decodes the record whose body starts at `image[0]` (`P+4`) both ways: from the body slice, as the `pread`
    /// reader does, and word by word, as the zero-copy reader does, with the words taken from `image` up to
    /// `P+align8(len)` (so the last word holds whatever `image` has after the payload, as a mapping would).
    fn decode_both(
        len: u32,
        image: &[u8],
        seq: u64,
    ) -> (
        Result<RecordHeader, CorruptReason>,
        Result<RecordHeader, CorruptReason>,
    ) {
        let by_body = decode_record_body(len, &image[..len as usize - 4], seq).map(|(h, _)| h);
        let crc = read_u32(image, 0).unwrap();
        let words = if len >= RECORD_HEADER_LEN as u32 {
            words_of(&image[4..align8(u64::from(len)) as usize - 4])
        } else {
            Vec::new()
        };
        let by_words = decode_record_words(len, crc, crate::mmap::SharedWords::new(&words), seq);
        (by_body, by_words)
    }

    /// A record body for `payload_len` pseudorandom bytes, followed by 64 more pseudorandom bytes (padding and
    /// whatever comes next).
    fn record_image(payload_len: usize, seed: u64) -> (u32, Vec<u8>) {
        let mut x = seed | 1;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let payload: Vec<u8> = (0..payload_len).map(|_| next() as u8).collect();
        let header = RecordHeader {
            seq: 7,
            schema_id: next() as u32,
            venue_ts_ns: next(),
            local_ts_ns: next(),
        };
        let (len, mut image) = body(&header, &payload);
        image.extend((0..64).map(|_| next() as u8));
        (len, image)
    }

    #[test]
    fn word_decoder_matches_body_decoder_for_every_length() {
        // Every payload length through two full rounds of both CRC lane sizes plus every tail (the three-lane
        // thresholds are 192 and 768 bytes of payload words), so every alignment of the payload's last word.
        const MAX_PAYLOAD: usize = 2 * 3 * (256 + 64) + 64 + 16;
        let step = if cfg!(miri) { 199 } else { 1 };
        for payload_len in (0..=MAX_PAYLOAD).step_by(step) {
            let (len, image) = record_image(payload_len, payload_len as u64);
            let body_len = len as usize - 4;
            let padded = align8(u64::from(len)) as usize - 4;
            let (by_body, by_words) = decode_both(len, &image, 7);
            assert!(by_body.is_ok(), "payload {payload_len}");
            assert_eq!(by_words, by_body, "payload {payload_len}");
            // Every byte position class: each byte of the CRC and header fields, the payload's first and last
            // bytes and the bytes on either side of each word boundary near its ends and the lane boundaries,
            // and the padding (not checked by either decoder).
            let mut positions: Vec<usize> = (0..36).collect();
            for p in [0, 1, 7, 8, 9, 191, 192, 255, 256, 767, 768] {
                positions.push(36 + p);
            }
            for back in 1..=9 {
                positions.push(body_len.saturating_sub(back));
            }
            positions.extend(body_len..padded);
            positions.retain(|&p| p < padded);
            for p in positions {
                let mut bad = image.clone();
                bad[p] ^= 0x41;
                let (by_body, by_words) = decode_both(len, &bad, 7);
                assert_eq!(by_words, by_body, "payload {payload_len}, byte {p}");
                assert_eq!(
                    by_body.is_ok(),
                    p >= body_len,
                    "payload {payload_len}, byte {p}"
                );
                // The same damage under a recomputed CRC, so that the checks after the CRC decide: the sequence
                // number, the flags, or nothing (schema id, timestamps, payload and padding are accepted as found).
                finish_record_body(len, &mut bad[..body_len]);
                let (by_body, by_words) = decode_both(len, &bad, 7);
                assert_eq!(
                    by_words, by_body,
                    "payload {payload_len}, byte {p}, valid CRC"
                );
                let expected = match p {
                    4..12 => Err(CorruptReason::BadSequence {
                        expected: 7,
                        found: 7 ^ (0x41 << (8 * (p - 4))),
                    }),
                    16..20 => Err(CorruptReason::BadFlags),
                    _ => Ok(()),
                };
                assert_eq!(
                    by_body.map(|_| ()),
                    expected,
                    "payload {payload_len}, byte {p}, valid CRC"
                );
            }
            // A wrong sequence number, and commit words that disagree with the bytes.
            assert_eq!(decode_both(len, &image, 8).1, decode_both(len, &image, 8).0);
            for other in [
                len.wrapping_sub(8),
                len - 1,
                len + 1,
                len + 8,
                0,
                39,
                SEAL_MARKER,
            ] {
                if (other as usize) <= image.len() - 4 && other != 0 && other != SEAL_MARKER {
                    let (by_body, by_words) = decode_both(other, &image, 7);
                    assert_eq!(
                        by_words, by_body,
                        "payload {payload_len}, commit word {other}"
                    );
                    assert!(
                        by_words.is_err(),
                        "payload {payload_len}, commit word {other}"
                    );
                }
            }
        }
    }

    #[test]
    fn word_decoder_validation_rules() {
        let (len, image) = record_image(3, 1);
        let crc = read_u32(&image, 0).unwrap();
        let words = words_of(&image[4..align8(u64::from(len)) as usize - 4]);
        let view = crate::mmap::SharedWords::new(&words);
        assert_eq!(decode_record_words(len, crc, view, 7).map(|h| h.seq), Ok(7));
        for bad in [0, 1, 39, SEAL_MARKER] {
            assert_eq!(
                decode_record_words(bad, crc, view, 7),
                Err(CorruptReason::BadLength(bad))
            );
        }
        // Word counts that don't match the commit word, one short and one long.
        for (short, long) in [
            (view.split_at_checked(view.len() - 1).unwrap().0, None),
            (view, Some(())),
        ] {
            let extended;
            let words = match long {
                None => short,
                Some(()) => {
                    extended = words_of(&image[4..align8(u64::from(len)) as usize + 4]);
                    crate::mmap::SharedWords::new(&extended)
                }
            };
            assert_eq!(
                decode_record_words(len, crc, words, 7),
                Err(CorruptReason::BadLength(len))
            );
        }
        assert_eq!(
            decode_record_words(len, crc ^ 1, view, 7),
            Err(CorruptReason::BadChecksum)
        );
        assert_eq!(
            decode_record_words(len, crc, view, 8),
            Err(CorruptReason::BadSequence {
                expected: 8,
                found: 7
            })
        );
        // Every flag bit on its own, under a valid CRC: both decoders reject each one.
        for bit in 0..32 {
            let mut flagged = image[..len as usize - 4].to_vec();
            flagged[16 + bit / 8] |= 1 << (bit % 8);
            finish_record_body(len, &mut flagged);
            let words = words_of_padded(&flagged[4..], 0x5c);
            let crc = read_u32(&flagged, 0).unwrap();
            let view = crate::mmap::SharedWords::new(&words);
            assert_eq!(
                decode_record_words(len, crc, view, 7),
                Err(CorruptReason::BadFlags),
                "flag bit {bit}"
            );
            assert_eq!(
                decode_record_body(len, &flagged, 7).map(|(h, _)| h),
                Err(CorruptReason::BadFlags),
                "flag bit {bit}"
            );
            // A wrong sequence number as well: the sequence is checked first, by both decoders.
            assert_eq!(
                decode_record_words(len, crc, view, 8),
                Err(CorruptReason::BadSequence {
                    expected: 8,
                    found: 7
                }),
                "flag bit {bit} and a wrong sequence number"
            );
            assert_eq!(
                decode_record_body(len, &flagged, 8).map(|(h, _)| h),
                Err(CorruptReason::BadSequence {
                    expected: 8,
                    found: 7
                }),
                "flag bit {bit} and a wrong sequence number"
            );
        }
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
        assert_eq!(
            read_slot(&image[..], 64, 7),
            Err(CorruptReason::BadSeal),
            "a seal at the first slot names its own segment"
        );
        seal(&mut image, 128, 7);
        assert_eq!(read_slot(&image[..], 128, 7), Ok(Slot::Seal));
        assert_eq!(read_slot(&image[..], 128, 8), Err(CorruptReason::BadSeal));
        assert_eq!(
            read_slot(&image[..132], 128, 7),
            Err(CorruptReason::BadSeal)
        );
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
        // Equal generations (never written by tickertape): the slot at 65,536 wins (docs/format.md).
        let tie = CheckpointSlot {
            next_seq: 99,
            ..checkpoint(2)
        };
        file[65_536..65_600].copy_from_slice(&encode_checkpoint_slot(&tie));
        assert_eq!(decode_checkpoint(&file), Some(tie));
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

    #[test]
    #[cfg_attr(miri, ignore = "scans megabytes")]
    fn nonzero_end_finds_the_last_non_zero_byte() {
        let mut image = vec![0u8; 3 << 20];
        assert_eq!(nonzero_end(&image[..], 0), None);
        assert_eq!(nonzero_end(&image[..], u64::MAX), None);
        for at in [
            0,
            7,
            8,
            1 << 20,
            (1 << 20) - 1,
            (3 << 20) - 1,
            (3 << 20) - 9,
        ] {
            image.fill(0);
            image[at] = 1;
            assert_eq!(nonzero_end(&image[..], 0), Some(at as u64 + 1), "{at}");
            assert_eq!(nonzero_end(&image[..], at as u64 + 1), None, "{at}");
        }
        image[5] = 1;
        assert_eq!(nonzero_end(&image[..], 6), Some((3 << 20) - 8));
    }

    /// A segment image with `n` records of varied lengths from offset 64, base 7. Returns it and the offsets.
    fn records_image(n: usize) -> (Vec<u8>, Vec<usize>) {
        let mut image = vec![0; 65_536];
        let mut offsets = Vec::new();
        let mut offset = 64;
        for i in 0..n {
            offsets.push(offset);
            offset = append(
                &mut image,
                offset,
                &record(7 + i as u64),
                &vec![i as u8; (i * 37) % 200],
            );
        }
        offsets.push(offset);
        (image, offsets)
    }

    #[test]
    fn history_after_a_zeroed_commit_word() {
        let (mut image, offsets) = records_image(20);
        // No history after the real end.
        assert_eq!(
            find_history_after_empty(&image[..], offsets[20] as u64, 27),
            None
        );
        // A zeroed commit word hides the records after it; the first one is the evidence.
        image[offsets[5]..offsets[5] + 4].fill(0);
        assert_eq!(
            find_history_after_empty(&image[..], offsets[5] as u64, 12),
            Some(LaterHistory::Record {
                offset: offsets[6] as u64,
                seq: 13
            })
        );
        // Wider damage: everything from record 5 up to the middle of record 9 zeroed.
        image[offsets[5]..offsets[9] + 20].fill(0);
        assert_eq!(
            find_history_after_empty(&image[..], offsets[5] as u64, 12),
            Some(LaterHistory::Record {
                offset: offsets[10] as u64,
                seq: 17
            })
        );
        // Records whose sequence numbers are not after the expected one, or further ahead than the space between
        // allows, are not evidence.
        assert_eq!(
            find_history_after_empty(&image[..], offsets[5] as u64, 26),
            None
        );
        assert_eq!(
            find_history_after_empty(&image[..], offsets[18] as u64, 0),
            None
        );
        // The last record before a seal: the final seal is the evidence.
        let (mut sealed, offsets) = records_image(3);
        seal(&mut sealed, offsets[3], 10);
        sealed[offsets[2]..offsets[2] + 4].fill(0);
        assert_eq!(
            find_history_after_empty(&sealed[..], offsets[2] as u64, 9),
            Some(LaterHistory::Seal {
                offset: offsets[3] as u64,
                next_seq: 10
            })
        );
        // A seal that is not the last non-zero item, or names a sequence number out of range, is not.
        assert_eq!(
            find_history_after_empty(&sealed[..], offsets[2] as u64, 10),
            None
        );
        sealed[offsets[3] + 16] = 1;
        assert_eq!(
            find_history_after_empty(&sealed[..], offsets[2] as u64, 9),
            None
        );
    }

    #[test]
    fn final_seal_search_is_capped() {
        // Review round 2, item 3: a final seal is evidence only if it announces one of the `SEAL_SEARCH_RECORDS`
        // sequence numbers after the expected one, however far from the empty word it lies. Record 9's commit word
        // is zeroed, and a seal follows 8,000 bytes later (room for 200 minimal records).
        let (mut image, offsets) = records_image(3);
        let p = offsets[2];
        image[p..p + 4].fill(0);
        let q = offsets[3] + 8_000;
        for (k, found) in [
            (1, true),
            (SEAL_SEARCH_RECORDS, true),
            (SEAL_SEARCH_RECORDS + 1, false),
            (150, false),
        ] {
            let mut copy = image.clone();
            seal(&mut copy, q, 9 + k);
            let expect = found.then_some(LaterHistory::Seal {
                offset: q as u64,
                next_seq: 9 + k,
            });
            assert_eq!(
                find_history_after_empty(&copy[..], p as u64, 9),
                expect,
                "{k}"
            );
        }
        // Sequence numbers at the end of the range: no overflow, nothing to find past `u64::MAX`.
        let mut copy = image.clone();
        seal(&mut copy, q, u64::MAX);
        assert_eq!(
            find_history_after_empty(&copy[..], p as u64, u64::MAX - 1),
            Some(LaterHistory::Seal {
                offset: q as u64,
                next_seq: u64::MAX
            })
        );
        assert_eq!(
            find_history_after_empty(&copy[..], p as u64, u64::MAX),
            None
        );
        assert_eq!(find_history_after_empty(&copy[..], u64::MAX - 7, 9), None);
    }

    #[test]
    fn crc_length_search_matches_direct_checksums() {
        let mut image = vec![0u8; 4096];
        let mut x = 0x2545_f491_4f6c_dd1d_u64;
        for b in &mut image[72..1000] {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        let at = 64_u64;
        let step = if cfg!(miri) { 97 } else { 1 };
        for len in (40..1100_u64).step_by(step) {
            let stored = record_crc(len as u32, &image[72..(at + len) as usize]);
            assert!(
                crc_matches_some_len(&image[..], at, stored, len, len),
                "{len}"
            );
            assert!(
                crc_matches_some_len(&image[..], at, stored, 40, len + 30),
                "{len}"
            );
            assert!(crc_matches_some_len(
                &image[..],
                at,
                stored,
                len.saturating_sub(5),
                len + 5
            ));
            assert!(
                !crc_matches_some_len(&image[..], at, stored, len + 1, len + 1),
                "{len}"
            );
            assert!(
                !crc_matches_some_len(&image[..], at, stored ^ 1, len, len),
                "{len}"
            );
        }
        // Lengths that don't fit in the segment are never tried.
        let len = 4096 - 8 - 64;
        let stored = record_crc(len as u32, &image[72..4096 - 8]);
        assert!(crc_matches_some_len(&image[..], at, stored, len, len + 100));
        assert!(!crc_matches_some_len(
            &image[..],
            at + 8,
            stored,
            len,
            len + 100
        ));
    }

    /// A view that records the largest range it was asked for, as a `pread` view would have to allocate it, and
    /// announces the `pread` view's window as its [`SegmentView::max_piece`].
    #[cfg(not(miri))]
    struct Metered<'a> {
        bytes: &'a [u8],
        largest: std::cell::Cell<u64>,
    }

    #[cfg(not(miri))]
    impl<'a> Metered<'a> {
        /// The `pread` view's window (`journal::SCAN_WINDOW`).
        const WINDOW: u64 = 1 << 20;

        fn new(bytes: &'a [u8]) -> Self {
            Metered {
                bytes,
                largest: std::cell::Cell::new(0),
            }
        }

        fn assert_bounded(&self) {
            assert!(
                self.largest.get() <= Self::WINDOW,
                "read {} bytes at once",
                self.largest.get()
            );
        }
    }

    #[cfg(not(miri))]
    impl SegmentView for Metered<'_> {
        fn len(&self) -> u64 {
            self.bytes.len() as u64
        }

        fn commit_word(&self, offset: u64) -> Option<u32> {
            self.bytes.commit_word(offset)
        }

        fn with_bytes<R>(&self, offset: u64, len: u64, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
            self.largest.set(self.largest.get().max(len));
            self.bytes.with_bytes(offset, len, f)
        }

        fn max_piece(&self) -> u64 {
            Self::WINDOW
        }
    }

    /// A 4 MiB segment image of base 7: one small record, then record 8 with a payload of about 3 MiB, then record 9.
    /// Returns it and the three offsets and the end.
    #[cfg(not(miri))]
    fn image_with_a_long_record() -> (Vec<u8>, [usize; 4]) {
        let mut image = vec![0u8; 4 << 20];
        let mut x = 0x2545_f491_4f6c_dd1d_u64;
        let long: Vec<u8> = (0..(3 << 20) + 13)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let r7 = 64;
        let r8 = append(&mut image, r7, &record(7), &[1; 20]);
        let r9 = append(&mut image, r8, &record(8), &long);
        let end = append(&mut image, r9, &record(9), &[2; 30]);
        (image, [r7, r8, r9, end])
    }

    #[test]
    #[cfg(not(miri))]
    fn forward_scan_reads_a_long_record_in_bounded_pieces() {
        // Review round 2, item 2: a `pread` view must never be asked for a whole record body larger than its window,
        // valid or not, because it allocates what it is asked for before the CRC can reject a damaged length.
        let (mut image, [_, r8, _, end]) = image_with_a_long_record();
        let view = Metered::new(&image);
        let expected = ScanResult {
            end_offset: end as u64,
            next_seq: 10,
            records: 3,
            end: ScanEnd::Empty,
        };
        assert_eq!(scan_segment(&view, 64, 7), expected);
        assert_eq!(scan_segment(&image[..], 64, 7), expected);
        view.assert_bounded();
        // The same record with one payload byte damaged, or its sequence number or flags wrong: the same errors as
        // over memory, still in bounded pieces.
        for (at, xor) in [
            (r8 + 1000, 1_u8),
            (r8 + (2 << 20), 0x80),
            (r8 + 40 + (3 << 20) + 12, 4),
            (r8 + 8, 1),
            (r8 + 20, 1),
        ] {
            let mut copy = image.clone();
            copy[at] ^= xor;
            let view = Metered::new(&copy);
            let scan = scan_segment(&view, 64, 7);
            assert_eq!(scan, scan_segment(&copy[..], 64, 7), "{at}");
            assert!(matches!(scan.end, ScanEnd::Invalid(_)), "{at}: {scan:?}");
            assert_eq!(scan.end_offset, r8 as u64);
            view.assert_bounded();
        }
        // A damaged length: record 7's commit word claims most of the segment.
        let claim = ((4 << 20) - 64 - 8_u32) & !7;
        image[64..68].copy_from_slice(&claim.to_le_bytes());
        let view = Metered::new(&image);
        assert_eq!(
            scan_segment(&view, 64, 7).end,
            ScanEnd::Invalid(CorruptReason::BadChecksum)
        );
        view.assert_bounded();
    }

    #[test]
    #[cfg(not(miri))]
    fn candidate_validation_reads_a_long_frame_in_bounded_pieces() {
        // Review round 2, item 2: after an empty commit word, a candidate frame with an in-range sequence number,
        // zero flags and a large length that fits is validated without asking for its whole body.
        let (image, [r7, r8, ..]) = image_with_a_long_record();
        // Record 7's commit word zeroed: the long record 8 is the evidence.
        let mut damaged = image.clone();
        damaged[r7..r7 + 4].fill(0);
        let view = Metered::new(&damaged);
        assert_eq!(
            find_history_after_empty(&view, r7 as u64, 7),
            Some(LaterHistory::Record {
                offset: r8 as u64,
                seq: 8
            })
        );
        view.assert_bounded();
        // Its body damaged too: it no longer validates, and record 9 after it is the evidence.
        damaged[r8 + 5000] ^= 1;
        let view = Metered::new(&damaged);
        assert!(matches!(
            find_history_after_empty(&view, r7 as u64, 7),
            Some(LaterHistory::Record { seq: 9, .. })
        ));
        view.assert_bounded();
    }

    #[test]
    #[cfg(not(miri))]
    fn piecewise_validation_decodes_the_header_the_crc_covered() {
        // Review round 3, item 1: a `pread` view rereads the file, which may change between two reads (a writer
        // reusing the slot while `verify` or a scan runs). The header returned must come from the bytes the CRC
        // covered: here the header fields change once they have been read, and a second read would see the new ones.
        struct Changing<'a> {
            before: &'a [u8],
            after: &'a [u8],
            fields: std::ops::Range<u64>,
            reads: std::cell::Cell<u32>,
        }
        impl SegmentView for Changing<'_> {
            fn len(&self) -> u64 {
                self.before.len() as u64
            }
            fn commit_word(&self, offset: u64) -> Option<u32> {
                self.before.commit_word(offset)
            }
            fn with_bytes<R>(
                &self,
                offset: u64,
                len: u64,
                f: impl FnOnce(&[u8]) -> R,
            ) -> Option<R> {
                let bytes = if self.reads.get() == 0 {
                    self.before
                } else {
                    self.after
                };
                if offset < self.fields.end && offset.saturating_add(len) > self.fields.start {
                    self.reads.set(self.reads.get() + 1);
                }
                bytes.with_bytes(offset, len, f)
            }
            fn max_piece(&self) -> u64 {
                Metered::WINDOW
            }
        }
        let (image, [_, r8, r9, _]) = image_with_a_long_record();
        let mut after = image.clone();
        after[r8 + 36] ^= 1; // Record 8's local timestamp.
        let view = Changing {
            before: &image,
            after: &after,
            fields: r8 as u64 + 8..r8 as u64 + RECORD_HEADER_LEN as u64,
            reads: std::cell::Cell::new(0),
        };
        let expected = read_slot(&image[..], r8 as u64, 8);
        assert!(
            matches!(expected, Ok(Slot::Record { next_offset, .. }) if next_offset == r9 as u64)
        );
        assert_eq!(read_slot(&view, r8 as u64, 8), expected);
        assert_eq!(
            view.reads.get(),
            1,
            "the header fields were read more than once"
        );
    }

    #[test]
    #[cfg(not(miri))]
    fn crc_length_search_reads_long_frames_in_bounded_pieces() {
        // A frame of about 3 MiB: `verify` meets this size when it weighs the tail after an empty commit word.
        let mut image = vec![0u8; (3 << 20) + 4096];
        let mut x = 0x9e37_79b9_7f4a_7c15_u64;
        for b in &mut image[72..(3 << 20) + 1000] {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        let at = 64_u64;
        let len = (3 << 20) + 936_u64;
        let stored = record_crc(len as u32, &image[72..(at + len) as usize]);
        let view = Metered::new(&image);
        assert!(crc_matches_some_len(&view, at, stored, len - 40, len + 40));
        assert!(!crc_matches_some_len(
            &view,
            at,
            stored ^ 1,
            len - 40,
            len + 40
        ));
        view.assert_bounded();
    }

    #[test]
    fn frames_inside_a_completely_written_torn_frame_are_not_history() {
        // Records 7 to 9, then a torn append of record 10 whose payload holds the frames of records 10 to 14 of
        // another journal (or of this one's future), at aligned positions. Records 11 to 14 fall in the window.
        let (mut image, offsets) = records_image(3);
        let p = offsets[3];
        let mut frames = vec![0u8; 1200];
        let mut at = 0;
        for i in 0..5 {
            at = append(
                &mut frames,
                at,
                &record(10 + i),
                &vec![i as u8; (i as usize * 37) % 200],
            );
        }
        frames.truncate(at);
        let mut payload = frames.clone();
        payload.push(0xee);
        let (len, torn) = body(&record(10), &payload);
        image[p + 4..p + len as usize].copy_from_slice(&torn);
        // Complete body, CRC stored: the candidates lie inside it.
        assert_eq!(find_history_after_empty(&image[..], p as u64, 10), None);
        // The same with up to COVER_SLACK trailing zero bytes in the payload, and with one more.
        for zeros in [1, 8, COVER_SLACK as usize, COVER_SLACK as usize + 1] {
            let mut copy = image.clone();
            copy[p..p + len as usize + 200].fill(0);
            let mut padded = payload.clone();
            padded.extend(std::iter::repeat_n(0, zeros));
            let (plen, pbody) = body(&record(10), &padded);
            copy[p + 4..p + plen as usize].copy_from_slice(&pbody);
            let found = find_history_after_empty(&copy[..], p as u64, 10);
            if zeros as u64 <= COVER_SLACK {
                assert_eq!(found, None, "{zeros}");
            } else {
                assert!(
                    matches!(found, Some(LaterHistory::Record { seq: 11, .. })),
                    "{zeros}: {found:?}"
                );
            }
        }
        // Torn in the middle: no CRC yet. Evidence (the accepted false positive).
        let mut copy = image.clone();
        copy[p + 4..p + 8].fill(0);
        copy[p + 40 + frames.len() / 2 + 64..p + len as usize].fill(0);
        assert!(matches!(
            find_history_after_empty(&copy[..], p as u64, 10),
            Some(LaterHistory::Record { seq: 11, .. })
        ));
        // A frame at P with the wrong sequence number or non-zero flags is not a torn append of record 10.
        for (field, value) in [(8, 99_u8), (20, 1)] {
            let mut copy = image.clone();
            copy[p + field] = value;
            assert!(
                find_history_after_empty(&copy[..], p as u64, 10).is_some(),
                "{field}"
            );
        }
        // A real zeroed commit word in front of the newest two records is still found: the frame at P is a complete
        // record that ends where the next one starts.
        let (mut real, offsets) = records_image(6);
        real[offsets[4]..offsets[4] + 4].fill(0);
        assert_eq!(
            find_history_after_empty(&real[..], offsets[4] as u64, 11),
            Some(LaterHistory::Record {
                offset: offsets[5] as u64,
                seq: 12
            })
        );
    }

    #[test]
    fn history_search_stops_at_its_work_bound() {
        // Every slot after the empty one carries the header of the next record, claiming the largest length, with a
        // bad checksum: checking them all would cost about (size / 8) * size bytes of CRC.
        let mut image = vec![0u8; 65_536];
        let mut q = 64 + 40;
        while q + 48 <= 65_536 - 8 {
            let len = (65_536 - 8 - q) as u32;
            image[q..q + 4].copy_from_slice(&len.to_le_bytes());
            image[q + 4..q + 8].copy_from_slice(&1_u32.to_le_bytes());
            image[q + 8..q + 16].copy_from_slice(&8_u64.to_le_bytes());
            q += 24;
            q = q.div_ceil(8) * 8;
        }
        let found = find_history_after_empty(&image[..], 64, 7);
        assert!(
            matches!(found, Some(LaterHistory::Unchecked { seq: 8, .. })),
            "{found:?}"
        );
        // Inconclusive, and reported as such (review round 2, item 6).
        assert_eq!(
            found.map(|h| h.reason()),
            Some(CorruptReason::UncheckedTail(8))
        );
        let proven = LaterHistory::Record { offset: 64, seq: 8 };
        assert_eq!(proven.reason(), CorruptReason::EmptyCommitWord(8));
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
        fn word_decoder_agrees_with_body_decoder(
            payload_len in 0_usize..2200, seed in any::<u64>(), byte in any::<usize>(), xor in any::<u8>(),
            seq_delta in 0_u64..3, len_delta in -9_i64..=9, fix_crc in 0_u8..3,
        ) {
            let (len, image) = record_image(payload_len, seed);
            let mut image = image;
            let padded = align8(u64::from(len)) as usize - 4;
            image[byte % padded] ^= xor;
            let other = u32::try_from(i64::from(len) + len_delta).unwrap();
            if other >= 40 && other as usize - 4 <= image.len() - 4 {
                // In a third of the cases, recompute the CRC for the commit word used, so that the damage reaches
                // the checks after the CRC (sequence number, flags) instead of almost always failing the CRC.
                if fix_crc == 0 {
                    finish_record_body(other, &mut image[..other as usize - 4]);
                }
                let (by_body, by_words) = decode_both(other, &image, 7 + seq_delta);
                if fix_crc == 0 {
                    prop_assert_ne!(&by_body, &Err(CorruptReason::BadChecksum));
                }
                prop_assert_eq!(by_words, by_body);
            }
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
            // A writer never seals an empty segment, and a seal at offset 64 is invalid.
            let sealed = sealed && !records.is_empty();
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

        #[test]
        fn zeroed_commit_word_is_found_and_torn_bodies_are_not(
            lens in prop::collection::vec(0_usize..600, 1..40),
            damaged in any::<prop::sample::Index>(),
            torn_len in 0_usize..3000,
            torn_seed in any::<u64>(),
        ) {
            // Committed records with one commit word zeroed: found exactly when a record follows.
            let mut image = vec![0; 65_536];
            image[..64].copy_from_slice(&encode_segment_header(&SegmentHeader { base_seq: 7, ..header() }));
            let mut offsets = Vec::new();
            let mut offset = 64;
            for (i, len) in lens.iter().enumerate() {
                if offset + align8(40 + *len as u64) as usize > 65_536 - 8 { break; }
                offsets.push(offset);
                offset = append(&mut image, offset, &record(7 + i as u64), &vec![i as u8 ^ 0x5a; *len]);
            }
            let end = offset;
            let k = damaged.index(offsets.len());
            let mut copy = image.clone();
            copy[offsets[k]..offsets[k] + 4].fill(0);
            let found = find_history_after_empty(&copy[..], offsets[k] as u64, 7 + k as u64);
            if k + 1 < offsets.len() {
                prop_assert_eq!(found, Some(LaterHistory::Record { offset: offsets[k + 1] as u64, seq: 8 + k as u64 }));
            } else {
                prop_assert_eq!(found, None);
            }
            // A torn append after the end: the header of the next record and arbitrary payload bytes, never evidence.
            let n = offsets.len() as u64;
            let room = 65_536 - 8 - end;
            if room >= 48 {
                let len = 40 + torn_len.min(room - 40);
                let mut x = torn_seed | 1;
                let payload: Vec<u8> = (0..len - 40).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect();
                let (_, torn_body) = body(&record(7 + n), &payload);
                let mut torn = image.clone();
                torn[end + 4..end + len].copy_from_slice(&torn_body);
                prop_assert_eq!(find_history_after_empty(&torn[..], end as u64, 7 + n), None);
                // A complete body (CRC stored) whose payload holds the frames of the records that would follow it,
                // as a payload copying frames from elsewhere does: they lie inside the frame, so not evidence.
                let mut frames = vec![0u8; len - 40];
                let mut at = 0;
                let mut k = 1;
                while at + 48 <= frames.len() {
                    at = append(&mut frames, at, &record(7 + n + k), &[k as u8; 8]);
                    k += 1;
                }
                if let Some(last) = frames.last_mut() { *last = 1; }
                let (_, embedding) = body(&record(7 + n), &frames);
                let mut torn = image.clone();
                torn[end + 4..end + len].copy_from_slice(&embedding);
                prop_assert_eq!(find_history_after_empty(&torn[..], end as u64, 7 + n), None);
            }
        }
    }
}
