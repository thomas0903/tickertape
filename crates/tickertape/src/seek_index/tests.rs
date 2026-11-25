//! The byte codec of the seek index, and the builder against generated segments (ADR 0045, test plan 1).

use std::path::{Path, PathBuf};

use proptest::prelude::*;

use super::codec::{self, Encoder, Entry, HEADER_LEN};
use super::*;
use crate::format::{RecordHeader, SegmentHeader};

/// A segment file image with these payloads as records, sealed, and where its records start (with their lengths).
fn image(size: u64, base: u64, created: u64, payloads: &[Vec<u8>]) -> (Vec<u8>, Vec<Entry>, u64) {
    let mut image = vec![0; size as usize];
    image[..64].copy_from_slice(&format::encode_segment_header(&SegmentHeader {
        segment_size: size,
        base_seq: base,
        created_unix_ns: created,
    }));
    let (mut at, mut seq, mut entries) = (64_usize, base, Vec::new());
    for payload in payloads {
        let len = format::frame_len(payload.len()).unwrap();
        let header = RecordHeader {
            seq,
            schema_id: 1,
            venue_ts_ns: seq,
            local_ts_ns: seq,
        };
        let end = at + len as usize;
        format::encode_record_body(&header, payload, &mut image[at + 4..end]);
        image[at..at + 4].copy_from_slice(&len.to_le_bytes());
        entries.push(Entry {
            seq,
            offset: at as u64,
            len,
            crc: u32::from_le_bytes(image[at + 4..at + 8].try_into().unwrap()),
        });
        at += format::align8(u64::from(len)) as usize;
        seq += 1;
    }
    assert!(at as u64 <= size - 8, "the records do not fit");
    image[at..at + 4].copy_from_slice(&format::SEAL_MARKER.to_le_bytes());
    image[at + 4..at + 8].copy_from_slice(&format::seal_crc(seq).to_le_bytes());
    (image, entries, at as u64)
}

fn write_segment(dir: &Path, base: u64, image: &[u8]) -> PathBuf {
    let path = journal::segment_path(dir, base);
    std::fs::write(&path, image).unwrap();
    path
}

fn index_path(dir: &Path, base: u64) -> PathBuf {
    dir.join(DIR_NAME).join(dir::index_name(base))
}

fn states(report: &[SegmentIndex]) -> Vec<IndexState> {
    report.iter().map(|s| s.state).collect()
}

const VECTOR: &str = "
54 49 43 4b 54 49 44 58 01 00 80 00 18 00 00 00
01 00 00 00 00 00 01 00 b8 ff 01 00 00 00 00 00
d3 08 00 00 00 00 00 00 00 00 00 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 c3 ec 87 c9
54 49 43 4b 54 41 50 45 01 00 40 00 00 00 00 00
00 00 02 00 00 00 00 00 e8 03 00 00 00 00 00 00
00 00 2a 36 fe 9c 97 17 00 00 00 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 5c f1 81 ff
5e 06 00 00 00 00 00 00 30 00 01 00 00 00 00 00
68 00 00 00 92 7f f1 5a";

fn vector() -> Vec<u8> {
    VECTOR
        .split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect()
}

/// The record's test vector, both ways: the builder writes exactly these bytes, and they decode to what they say.
#[test]
#[cfg_attr(miri, ignore = "files")]
fn test_vector() {
    let (image, entries, seal) = image(
        131_072,
        1000,
        1_700_000_000_000_000_000,
        &vec![vec![0x5A; 64]; 1259],
    );
    assert_eq!(seal, 131_000);
    let expected = vector();
    assert_eq!(expected.len(), 152);
    let index = codec::decode(&expected, image[..64].try_into().unwrap()).unwrap();
    assert_eq!(
        (index.seal_offset, index.next_seq, index.spacing),
        (131_000, 2259, 65_536)
    );
    assert_eq!(index.entries().collect::<Vec<_>>(), [entries[630]]);
    assert_eq!((entries[630].seq, entries[630].offset), (1630, 65_584));

    let dir = tempfile::tempdir().unwrap();
    write_segment(dir.path(), 1000, &image);
    let report = build(dir.path(), false).unwrap();
    assert_eq!(states(&report.segments), [IndexState::Built]);
    assert_eq!(
        std::fs::read(index_path(dir.path(), 1000)).unwrap(),
        expected
    );
    // Again: kept, and with `rebuild` written anew, the same bytes both times.
    for (rebuild, state) in [(false, IndexState::Valid), (true, IndexState::Built)] {
        assert_eq!(
            states(&build(dir.path(), rebuild).unwrap().segments),
            [state]
        );
        assert_eq!(
            std::fs::read(index_path(dir.path(), 1000)).unwrap(),
            expected
        );
    }
    assert_eq!(
        states(&check(dir.path()).unwrap().segments),
        [IndexState::Valid]
    );
    // Nothing else is left in the directory.
    assert_eq!(
        std::fs::read_dir(dir.path().join(DIR_NAME))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(remove(dir.path()).unwrap(), 1);
    assert_eq!(
        states(&check(dir.path()).unwrap().segments),
        [IndexState::Missing]
    );
}

/// A fence takes the next number where a name is taken, and the file it finds there stays.
#[test]
#[cfg_attr(miri, ignore = "files")]
fn a_taken_name_is_skipped() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join(DIR_NAME);
    std::fs::create_dir(&path).unwrap();
    let indexes = IndexDir::open(root.path()).unwrap().unwrap();
    let name = |n| dir::tmp_name(7, std::process::id(), n);
    for n in [10, 11] {
        std::fs::write(path.join(name(n)), b"another builder's").unwrap();
    }
    let fence = Fence::create(&indexes, 7, 10).unwrap();
    assert_eq!(fence.name, name(12));
    assert!(path.join(name(12)).exists());
    drop(fence);
    assert!(!path.join(name(12)).exists());
    for n in [10, 11] {
        assert_eq!(
            std::fs::read(path.join(name(n))).unwrap(),
            b"another builder's"
        );
    }
}

#[test]
fn spacing() {
    let gib = 1_u64 << 30;
    for (size, expected) in [
        (65_536, 65_536),
        (128 << 20, 65_536),
        (4 * gib, 65_536),
        (4 * gib + 65_536, 131_072),
        (8 * gib, 131_072),
        (8 * gib + 65_536, 262_144),
        (1 << 40, 16 << 20),
    ] {
        let s = codec::spacing(size);
        assert_eq!(s, expected, "{size}");
        assert!(size / u64::from(s) <= 65_536);
        assert!(codec::SPACING_RANGE.contains(&s));
    }
}

#[test]
fn lengths() {
    for (len, entries) in [
        (0, None),
        (127, None),
        (128, None),
        (151, None),
        (152, Some(1)),
        (153, None),
        (176, Some(2)),
        (codec::MAX_LEN as u64, Some(65_535)),
        (codec::MAX_LEN as u64 + 24, None),
        (u64::MAX, None),
    ] {
        assert_eq!(codec::entries_in(len), entries, "{len}");
    }
    assert_eq!(codec::MAX_LEN, 1_572_968);
}

/// A segment of 1 MiB with three indexed records, for the boundary tests below.
struct Sample {
    header: [u8; 64],
    bytes: Vec<u8>,
}

impl Sample {
    /// Entries at offsets 65,600 (seq 110), 131,200 (seq 120) and 200,000 (seq 130), 104 bytes each; base 100; seal at
    /// 300,000 for 140.
    fn new() -> Sample {
        let header = format::encode_segment_header(&SegmentHeader {
            segment_size: 1 << 20,
            base_seq: 100,
            created_unix_ns: 5,
        });
        let mut encoder = Encoder::new(1 << 20);
        for (seq, offset) in [(110, 65_600), (120, 131_200), (130, 200_000)] {
            assert!(encoder.wants(offset));
            encoder.push(Entry {
                seq,
                offset,
                len: 104,
                crc: 7,
            });
        }
        let bytes = encoder.finish(&header, 300_000, 140).unwrap();
        Sample { header, bytes }
    }

    fn decode(&self) -> std::result::Result<(), &'static str> {
        codec::decode(&self.bytes, &self.header).map(|_| ())
    }

    /// Whether the index decodes with `value` at `at` and a checksum that fits.
    fn with(&self, at: usize, value: &[u8]) -> bool {
        let mut other = self.bytes.clone();
        other[at..at + value.len()].copy_from_slice(value);
        codec::write_crc(&mut other);
        codec::decode(&other, &self.header).is_ok()
    }
}

/// Every rule of step 4, each at its boundary.
#[test]
fn decode_boundaries() {
    let s = Sample::new();
    assert_eq!(s.decode(), Ok(()));
    let entry = |i: usize| HEADER_LEN + i * 24;
    let u64s = |at: usize, cases: &[(u64, bool)]| {
        for (value, ok) in cases {
            assert_eq!(s.with(at, &value.to_le_bytes()), *ok, "{at}: {value}");
        }
    };
    let u32s = |at: usize, cases: &[(u32, bool)]| {
        for (value, ok) in cases {
            assert_eq!(s.with(at, &value.to_le_bytes()), *ok, "{at}: {value}");
        }
    };
    // Versions 0, 1 and 2; the lengths; the reserved fields.
    for (at, value, ok) in [
        (8, 0_u16, false),
        (8, 1, true),
        (8, 2, false),
        (10, 64, false),
        (10, 128, true),
        (12, 16, false),
        (12, 24, true),
        (14, 1, false),
    ] {
        assert_eq!(s.with(at, &value.to_le_bytes()), ok, "{at}: {value}");
    }
    for at in 40..60 {
        assert!(!s.with(at, &[1]), "reserved byte {at}");
    }
    for at in 0..8 {
        assert!(!s.with(at, &[0]), "magic byte {at}");
    }
    u32s(
        16,
        &[
            (0, false),
            (2, false),
            (3, true),
            (4, false),
            (u32::MAX, false),
        ],
    );
    // The spacing: a power of two from 4,096 to 2^30 in which the entries are in different blocks, from block 1 on.
    u32s(
        20,
        &[
            (0, false),
            (2_048, false),
            (4_096, true),
            (32_768, true),
            (65_535, false),
            (65_536, true),
            (65_537, false),
            (131_072, false),
            (1 << 31, false),
        ],
    );
    u64s(
        24,
        &[
            (0, false),
            (200_104, false), // room for 9 records of 40 bytes after the last entry is missing
            (200_104 + 9 * 40 - 8, false),
            (200_104 + 9 * 40, true),
            (200_104 + 9 * 40 + 1, false),
            (300_000, true),
            ((1 << 20) - 8, true),
            (1 << 20, false),
            (u64::MAX - 7, false),
        ],
    );
    u64s(
        32,
        &[
            (0, false),
            (100, false),
            (130, false),
            (131, true),
            (131 + 2_497, true), // (300,000 - 200,104) / 40 = 2,497 records
            (131 + 2_498, false),
            (u64::MAX, false),
        ],
    );
    // First entry: sequence number, offset, length.
    u64s(
        entry(0),
        &[
            (0, false),
            (100, false),
            (101, true),
            (119, true),
            (120, false),
            (u64::MAX, false),
        ],
    );
    u64s(
        entry(0) + 8,
        &[
            (0, false),
            (65_528, false), // block 0
            (65_536, true),
            (65_540, false),
            (131_064, false), // overlaps the next entry's place
            (u64::MAX - 7, false),
        ],
    );
    // 64 + 40 * (seq - base) <= offset, here for the second record of a block: 1,638 records fit before 65,600.
    assert!(!s.with(entry(0), &1_739_u64.to_le_bytes()));
    u32s(
        entry(0) + 16,
        &[
            (0, false),
            (39, false),
            (40, true),
            (65_600 - 9 * 40, true), // 131,200 - 65,600 - 9 records of 40 bytes
            (65_600 - 9 * 40 + 1, false),
            (u32::MAX - 1, false),
            (u32::MAX, false),
        ],
    );
    // Order.
    u64s(
        entry(1),
        &[
            (110, false),
            (111, true),
            (129, true),
            (130, false),
            (131, false),
        ],
    );
    u64s(
        entry(1) + 8,
        &[
            (65_600, false),
            (131_064, false),
            (131_072, true),
            (196_608, false),
        ],
    );
    u64s(
        entry(2),
        &[(120, false), (121, true), (139, true), (140, false)],
    );
    // Two entries in one block: 196,608 is the start of block 3.
    u64s(
        entry(2) + 8,
        &[(196_608, true), (196_600, false), (299_896, false)],
    );
    // The copied segment header, and each of its bytes.
    for at in 64..128 {
        let value = [s.bytes[at] ^ 1];
        assert!(!s.with(at, &value), "segment header byte {at}");
    }
    // Without a checksum that fits, no single bit may differ.
    for bit in 0..s.bytes.len() * 8 {
        let mut other = s.bytes.clone();
        other[bit / 8] ^= 1 << (bit % 8);
        assert!(codec::decode(&other, &s.header).is_err(), "bit {bit}");
    }
    for len in 0..s.bytes.len() {
        assert!(codec::decode(&s.bytes[..len], &s.header).is_err(), "{len}");
    }
    let mut longer = s.bytes.clone();
    longer.push(0);
    assert!(codec::decode(&longer, &s.header).is_err());
    assert!(codec::decode(&vec![0; s.bytes.len()], &s.header).is_err());
    // A segment header that is not one is refused even where both copies agree.
    let mut zeroed = s.bytes.clone();
    zeroed[64..128].fill(0);
    codec::write_crc(&mut zeroed);
    assert!(codec::decode(&zeroed, &[0; 64]).is_err());
}

#[test]
fn floor_finds_the_greatest_entry_at_or_below() {
    let s = Sample::new();
    let index = codec::decode(&s.bytes, &s.header).unwrap();
    for (target, seq) in [
        (0, None),
        (109, None),
        (110, Some(110)),
        (119, Some(110)),
        (120, Some(120)),
        (129, Some(120)),
        (130, Some(130)),
        (u64::MAX, Some(130)),
    ] {
        assert_eq!(index.floor(target).map(|e| e.seq), seq, "{target}");
    }
}

#[test]
fn far_estimates_the_distance_in_bytes() {
    // 100-byte records: 10 passed, 1,000 bytes. 2,622 records ahead are 262,200 bytes, over 256 KiB (262,144).
    assert!(!far(10 + 2_621, 0, 10, 64 + 1_000));
    assert!(far(10 + 2_622, 0, 10, 64 + 1_000));
    assert!(
        !far(u64::MAX, 5, 5, 64),
        "nothing passed, nothing to estimate from"
    );
    assert!(far(u64::MAX, 0, 1, 1 << 40));
    assert!(!far(3, 0, 10, 1_064), "behind the cursor");
}

#[derive(Debug, Clone)]
struct Shape {
    size: u64,
    payloads: Vec<Vec<u8>>,
}

/// Records of random lengths that fill part or all of a segment.
fn shape() -> impl Strategy<Value = Shape> {
    let lens = prop_oneof![
        3 => proptest::collection::vec(0_usize..200, 1..4_000),
        2 => proptest::collection::vec(0_usize..9_000, 1..300),
        1 => proptest::collection::vec(0_usize..200_000, 1..12),
    ];
    (1_u64..8, lens, any::<u8>()).prop_map(|(units, lens, fill)| {
        let size = units * SEGMENT_SIZE_UNIT;
        let (mut at, mut payloads) = (64, Vec::new());
        for (i, len) in lens.into_iter().enumerate() {
            let frame = format::align8(40 + len as u64);
            if at + frame > size - 8 {
                break;
            }
            at += frame;
            payloads.push(vec![fill.wrapping_add(i as u8); len]);
        }
        if payloads.is_empty() {
            payloads.push(Vec::new());
        }
        Shape { size, payloads }
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, failure_persistence: None, ..ProptestConfig::default() })]

    /// The entries are exactly the first record of each block from block 1 on, whatever the records' sizes; building
    /// again gives the same bytes; `check` agrees; another spacing is accepted and kept.
    #[test]
    #[cfg_attr(miri, ignore = "files")]
    fn entries_are_the_first_record_of_each_block(
        shape in shape(),
        base in prop_oneof![Just(0), 1_u64..1 << 60],
        finer in 12_u32..16,
    ) {
        let (image, records, seal) = image(shape.size, base, 9, &shape.payloads);
        let header: [u8; 64] = image[..64].try_into().unwrap();
        let dir = tempfile::tempdir().unwrap();
        write_segment(dir.path(), base, &image);
        let mut expected: Vec<Entry> = Vec::new();
        let mut block = 0;
        for record in &records {
            if record.offset / 65_536 > block {
                block = record.offset / 65_536;
                expected.push(*record);
            }
        }
        let report = build(dir.path(), false).unwrap();
        let path = index_path(dir.path(), base);
        if expected.is_empty() {
            prop_assert_eq!(states(&report.segments), [IndexState::NotNeeded]);
            prop_assert!(!path.exists());
            prop_assert_eq!(states(&check(dir.path()).unwrap().segments), [IndexState::NotNeeded]);
            return Ok(());
        }
        prop_assert_eq!(states(&report.segments), [IndexState::Built]);
        let first = std::fs::read(&path).unwrap();
        let index = codec::decode(&first, &header).unwrap();
        prop_assert_eq!(index.entries().collect::<Vec<_>>(), expected);
        prop_assert_eq!((index.seal_offset, index.next_seq), (seal, base + records.len() as u64));
        build(dir.path(), true).unwrap();
        prop_assert_eq!(&std::fs::read(&path).unwrap(), &first);
        prop_assert_eq!(states(&check(dir.path()).unwrap().segments), [IndexState::Valid]);

        let mut encoder = Encoder::with_spacing(1 << finer);
        for record in &records {
            if encoder.wants(record.offset) {
                encoder.push(*record);
            }
        }
        let other = encoder.finish(&header, seal, base + records.len() as u64).unwrap();
        std::fs::write(&path, &other).unwrap();
        prop_assert_eq!(states(&build(dir.path(), false).unwrap().segments), [IndexState::Valid]);
        prop_assert_eq!(std::fs::read(&path).unwrap(), other);
        prop_assert_eq!(states(&check(dir.path()).unwrap().segments), [IndexState::Valid]);
    }

    /// Any bytes decode or not, without a panic; what decodes obeys the rules a reader relies on.
    #[test]
    fn decode_takes_any_bytes(
        mut bytes in proptest::collection::vec(any::<u8>(), 0..400),
        fields in proptest::collection::vec((0_usize..200, any::<u64>()), 0..4),
        sample in any::<bool>(),
        crc in any::<bool>(),
    ) {
        let s = Sample::new();
        if sample {
            bytes = s.bytes.clone();
        }
        for (at, value) in fields {
            if at + 8 <= bytes.len() {
                bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
            }
        }
        if crc {
            codec::write_crc(&mut bytes);
        }
        if let Ok(index) = codec::decode(&bytes, &s.header) {
            let mut before = 100;
            for entry in index.entries() {
                prop_assert!(entry.seq > before && entry.seq < index.next_seq);
                prop_assert!(entry.offset % 8 == 0 && entry.len >= 40);
                prop_assert!(entry.offset + u64::from(entry.len) <= index.seal_offset);
                prop_assert!(index.seal_offset <= (1 << 20) - 8);
                prop_assert_eq!(index.floor(entry.seq), Some(entry));
                before = entry.seq;
            }
        }
    }
}
