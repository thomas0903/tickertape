//! The seek index file, version 1 (ADR 0045, `docs/format.md` → Seek index): pure functions over bytes. Hidden: public
//! only for the workspace's tests and the fuzz target, not covered by the compatibility promise.

use crate::crc;
use crate::format::{self, SEGMENT_HEADER_LEN, SEGMENT_SIZE_UNIT, align8};

/// The 8 magic bytes at the start of an index.
pub const MAGIC: [u8; 8] = *b"TICKTIDX";
/// The version of the index file this build writes, and the only one it reads.
pub const VERSION: u16 = 1;
/// Length of the index header.
pub const HEADER_LEN: usize = 128;
/// Length of an entry.
pub const ENTRY_LEN: usize = 24;
/// The most entries an index can have.
pub const MAX_ENTRIES: usize = 65_535;
/// The longest an index file can be.
pub const MAX_LEN: usize = HEADER_LEN + MAX_ENTRIES * ENTRY_LEN;
/// The smallest and the largest spacing a reader accepts.
pub const SPACING_RANGE: std::ops::RangeInclusive<u32> = 4_096..=1 << 30;

const CRC_AT: usize = 60;
const SEGMENT_HEADER_AT: usize = 64;
const FRAME_MIN: u64 = format::RECORD_HEADER_LEN as u64;

/// Where one record of the segment is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// Its sequence number.
    pub seq: u64,
    /// The offset of its commit word.
    pub offset: u64,
    /// Its commit word (the frame's length).
    pub len: u32,
    /// Its stored CRC32C.
    pub crc: u32,
}

impl Entry {
    /// The first 8 bytes of the record: its commit word and its CRC32C.
    pub fn first_bytes(&self) -> [u8; 8] {
        let mut b = [0; 8];
        b[..4].copy_from_slice(&self.len.to_le_bytes());
        b[4..].copy_from_slice(&self.crc.to_le_bytes());
        b
    }
}

/// The block size a builder uses for a segment of `segment_size` bytes: the smallest power of two that is at least
/// 64 KiB and at least `segment_size / 65_536`.
pub fn spacing(segment_size: u64) -> u32 {
    segment_size
        .div_ceil(SEGMENT_SIZE_UNIT)
        .max(SEGMENT_SIZE_UNIT)
        .checked_next_power_of_two()
        .and_then(|s| u32::try_from(s).ok())
        .unwrap_or(*SPACING_RANGE.end())
}

/// The length of an index with `entries` entries, or `None` if there can be no such index.
pub fn len_of(entries: usize) -> Option<usize> {
    (1..=MAX_ENTRIES)
        .contains(&entries)
        .then(|| HEADER_LEN + entries * ENTRY_LEN)
}

/// The number of entries of an index of `len` bytes, or `None` if no index has that length.
pub fn entries_in(len: u64) -> Option<usize> {
    let entries = len.checked_sub(HEADER_LEN as u64)?;
    if entries % ENTRY_LEN as u64 != 0 {
        return None;
    }
    let count = usize::try_from(entries / ENTRY_LEN as u64).ok()?;
    len_of(count).map(|_| count)
}

/// Builds an index, entry by entry.
#[derive(Debug)]
pub struct Encoder {
    bytes: Vec<u8>,
    spacing: u32,
    /// The block of the last entry, 0 before the first: block 0 gets none.
    block: u64,
}

impl Encoder {
    /// An encoder for a segment of `segment_size` bytes, with the builder's spacing.
    pub fn new(segment_size: u64) -> Encoder {
        Encoder::with_spacing(spacing(segment_size))
    }

    /// An encoder with another spacing (a power of two). Readers accept any in [`SPACING_RANGE`].
    pub fn with_spacing(spacing: u32) -> Encoder {
        Encoder {
            bytes: vec![0; HEADER_LEN],
            spacing,
            block: 0,
        }
    }

    /// Whether a record at `offset` is the first of its block, given that records are offered in order.
    pub fn wants(&self, offset: u64) -> bool {
        offset / u64::from(self.spacing) > self.block
    }

    /// Adds an entry.
    pub fn push(&mut self, entry: Entry) {
        self.block = entry.offset / u64::from(self.spacing);
        self.bytes.extend_from_slice(&entry.seq.to_le_bytes());
        self.bytes.extend_from_slice(&entry.offset.to_le_bytes());
        self.bytes.extend_from_slice(&entry.first_bytes());
    }

    /// The finished index of the segment whose first 64 bytes are `segment_header` and whose seal, at `seal_offset`,
    /// names `next_seq`. `None` if there is no entry (then the segment gets no index), or more than an index can have.
    pub fn finish(
        mut self,
        segment_header: &[u8; SEGMENT_HEADER_LEN],
        seal_offset: u64,
        next_seq: u64,
    ) -> Option<Vec<u8>> {
        let count = entries_in(self.bytes.len() as u64)?;
        let b = &mut self.bytes;
        b[0..8].copy_from_slice(&MAGIC);
        b[8..10].copy_from_slice(&VERSION.to_le_bytes());
        b[10..12].copy_from_slice(&(HEADER_LEN as u16).to_le_bytes());
        b[12..14].copy_from_slice(&(ENTRY_LEN as u16).to_le_bytes());
        b[16..20].copy_from_slice(&(count as u32).to_le_bytes());
        b[20..24].copy_from_slice(&self.spacing.to_le_bytes());
        b[24..32].copy_from_slice(&seal_offset.to_le_bytes());
        b[32..40].copy_from_slice(&next_seq.to_le_bytes());
        b[SEGMENT_HEADER_AT..HEADER_LEN].copy_from_slice(segment_header);
        write_crc(b);
        Some(self.bytes)
    }
}

/// Computes `file_crc` over an index's bytes and stores it (tests use it on altered indexes too).
pub fn write_crc(index: &mut [u8]) {
    if index.len() >= HEADER_LEN {
        let crc = file_crc(index);
        index[CRC_AT..CRC_AT + 4].copy_from_slice(&crc.to_le_bytes());
    }
}

/// CRC32C of bytes `[0, 60)` followed by `[64, len)`. `index` is at least a header long.
fn file_crc(index: &[u8]) -> u32 {
    crc::crc32c_append(crc::crc32c(&index[..CRC_AT]), &index[SEGMENT_HEADER_AT..])
}

/// The magic and version at the start of `head`, if the magic is right: `Some(version)`.
pub fn version_of(head: &[u8]) -> Option<u16> {
    (head.get(..8)? == MAGIC).then_some(u16_at(head, 8)?)
}

/// A decoded index: everything [`decode`] checks holds.
#[derive(Debug, Clone, Copy)]
pub struct Index<'a> {
    /// The offset of the segment's seal.
    pub seal_offset: u64,
    /// The sequence number the seal names.
    pub next_seq: u64,
    /// The block size.
    pub spacing: u32,
    entries: &'a [u8],
}

impl<'a> Index<'a> {
    /// The entries, in order.
    pub fn entries(&self) -> impl DoubleEndedIterator<Item = Entry> + 'a {
        self.entries.chunks_exact(ENTRY_LEN).filter_map(entry_at)
    }

    /// The entry with the greatest `seq` at or below `target`.
    pub fn floor(&self, target: u64) -> Option<Entry> {
        let (mut low, mut high) = (0, self.entries.len() / ENTRY_LEN);
        // The entries before `low` are at or below the target, those from `high` on above it.
        while low < high {
            let mid = low + (high - low) / 2;
            if self.get(mid)?.seq <= target {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        self.get(low.checked_sub(1)?)
    }

    fn get(&self, i: usize) -> Option<Entry> {
        let at = i * ENTRY_LEN;
        entry_at(self.entries.get(at..at + ENTRY_LEN)?)
    }
}

/// Decodes an index file and makes every check that needs nothing but the file and the segment's first 64 bytes
/// (ADR 0045, step 4). The error says what is wrong. Allocates nothing.
pub fn decode<'a>(
    b: &'a [u8],
    segment_header: &[u8; SEGMENT_HEADER_LEN],
) -> Result<Index<'a>, &'static str> {
    const SHORT: &str = "its length is not that of an index";
    let count = entries_in(b.len() as u64).ok_or(SHORT)?;
    if version_of(b).ok_or("not an index (magic)")? != VERSION {
        return Err("another version of the index file");
    }
    if u16_at(b, 10) != Some(HEADER_LEN as u16)
        || u16_at(b, 12) != Some(ENTRY_LEN as u16)
        || u16_at(b, 14) != Some(0)
        || b[40..CRC_AT].iter().any(|&x| x != 0)
    {
        return Err("a fixed or reserved field of its header is wrong");
    }
    if u32_at(b, 16) != Some(count as u32) {
        return Err("its entry count does not match its length");
    }
    let spacing = u32_at(b, 20).ok_or(SHORT)?;
    if !spacing.is_power_of_two() || !SPACING_RANGE.contains(&spacing) {
        return Err("its spacing is not one an index can have");
    }
    if u32_at(b, CRC_AT) != Some(file_crc(b)) {
        return Err("its checksum does not match");
    }
    if b[SEGMENT_HEADER_AT..HEADER_LEN] != segment_header[..] {
        return Err("it belongs to another segment file (header)");
    }
    let header = format::decode_segment_header(segment_header)
        .map_err(|_| "the segment header in it is invalid")?;
    let seal_offset = u64_at(b, 24).ok_or(SHORT)?;
    let next_seq = u64_at(b, 32).ok_or(SHORT)?;
    let first = SEGMENT_HEADER_LEN as u64;
    if seal_offset % 8 != 0
        || seal_offset < first + FRAME_MIN
        || seal_offset > header.segment_size - format::SEAL_LEN as u64
        || next_seq <= header.base_seq
    {
        return Err("the seal it names cannot be in the segment");
    }
    let entries = &b[HEADER_LEN..];
    // Where the last record before the one being checked ends, and the sequence number after it; and its block.
    let (mut end, mut seq, mut block) = (first, header.base_seq, 0);
    for entry in entries.chunks_exact(ENTRY_LEN).filter_map(entry_at) {
        let frame_end = align8(u64::from(entry.len)).checked_add(entry.offset);
        let Some(frame_end) = frame_end.filter(|e| *e <= seal_offset) else {
            return Err("an entry is outside the segment's records");
        };
        if entry.offset % 8 != 0
            || u64::from(entry.len) < FRAME_MIN
            || entry.len > format::MAX_FRAME_LEN
            || entry.seq <= header.base_seq
            || entry.seq >= next_seq
        {
            return Err("an entry is not one a record can have");
        }
        // `spacing > 0`. Blocks before the first entry's are block 0, which has no entry: so `offset >= spacing`.
        if entry.offset / u64::from(spacing) <= block || !room(end, seq, entry.offset, entry.seq) {
            return Err("its entries are not in an order records can have");
        }
        block = entry.offset / u64::from(spacing);
        end = frame_end;
        seq = entry.seq + 1;
    }
    if !room(end, seq, seal_offset, next_seq) {
        return Err("its last entry does not fit before the seal");
    }
    Ok(Index {
        seal_offset,
        next_seq,
        spacing,
        entries,
    })
}

/// Whether the records `seq..next_seq` can lie in `[end, offset)`: at least 40 bytes each, and none if there are none
/// (then the space may still be zero: `end <= offset`).
fn room(end: u64, seq: u64, offset: u64, next_seq: u64) -> bool {
    next_seq
        .checked_sub(seq)
        .and_then(|records| records.checked_mul(FRAME_MIN))
        .and_then(|bytes| bytes.checked_add(end))
        .is_some_and(|least| least <= offset)
}

fn entry_at(b: &[u8]) -> Option<Entry> {
    Some(Entry {
        seq: u64_at(b, 0)?,
        offset: u64_at(b, 8)?,
        len: u32_at(b, 16)?,
        crc: u32_at(b, 20)?,
    })
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}
