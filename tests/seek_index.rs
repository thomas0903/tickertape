//! Seek indexes (ADR 0045, crash contract C15): a seek through an index lands where a scan lands, whatever the index
//! file holds, and falls back to what it does without one; the builder, the writer's open and retention keep indexes
//! and segments together. "Plan n" is item n of the record's test plan.

use std::cell::Cell;
use std::ffi::OsString;
use std::fs::Permissions;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use tickertape::fault::{self, Point};
use tickertape::retention::{self, Action, Policy};
use tickertape::seek_index::codec::{self, Encoder, Entry};
use tickertape::seek_index::{self, IndexState};
use tickertape::{
    Error, NewRecord, OwnedRecord, Reader, ReaderConfig, RecoveryPolicy, Writer, WriterConfig,
    format,
};
use tickertape_tests::{SMALL_SEGMENT, open_raw, segments, zero_commit_words_from};

const FIRST_SLOT: u64 = format::SEGMENT_HEADER_LEN as u64;
/// Four blocks, so up to three entries.
const SEGMENT: u64 = 4 * SMALL_SEGMENT;
/// The kinds of reader: (mapped, durable-only).
const KINDS: [(bool, bool); 4] = [(false, false), (true, false), (false, true), (true, true)];

fn config() -> WriterConfig {
    WriterConfig::default()
        .segment_size(SEGMENT)
        .prefault(false)
        .notify(false)
}

fn open_with(dir: &Path, (mapped, durable): (bool, bool), index: bool) -> Reader {
    let config = ReaderConfig::default()
        .durable_only(durable)
        .notify(false)
        .seek_index(index);
    if mapped {
        // SAFETY: only tickertape writers modify the segment files in these tests, except the tests' own damage, while
        // no reader holds a payload and no writer runs.
        unsafe { Reader::open_mapped(dir, config) }.unwrap()
    } else {
        Reader::open(dir, config).unwrap()
    }
}

fn open(dir: &Path, kind: (bool, bool)) -> Reader {
    open_with(dir, kind, true)
}

fn offset(reader: &Reader) -> u64 {
    reader.cursor().position.offset
}

/// Appends a record whose payload is `len` times `tag`.
fn append(writer: &mut Writer, tag: u8, len: usize) {
    let seq = writer.next_seq();
    writer
        .append(&NewRecord {
            schema_id: u32::from(tag),
            venue_ts_ns: seq,
            local_ts_ns: 0,
            payload: &vec![tag; len],
        })
        .unwrap();
}

/// Appends records of `len` bytes until `count` segments are sealed; returns how many records there are.
fn fill(writer: &mut Writer, dir: &Path, tag: u8, len: usize, count: usize) -> u64 {
    while segments(dir).len() <= count {
        for _ in 0..8 {
            append(writer, tag, len);
        }
    }
    writer.next_seq()
}

/// A checkpointed journal of `sealed` sealed segments with `len`-byte payloads, and their indexes.
fn indexed(len: usize, sealed: usize) -> (tempfile::TempDir, Writer) {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    fill(&mut writer, dir.path(), 1, len, sealed);
    writer.checkpoint().unwrap();
    build(dir.path());
    (dir, writer)
}

fn build(dir: &Path) -> Vec<IndexState> {
    states(&seek_index::build(dir, false).unwrap().segments)
}

fn check(dir: &Path) -> Vec<IndexState> {
    states(&seek_index::check(dir).unwrap().segments)
}

fn states(segments: &[seek_index::SegmentIndex]) -> Vec<IndexState> {
    segments.iter().map(|s| s.state).collect()
}

fn index_dir(dir: &Path) -> PathBuf {
    dir.join(seek_index::DIR_NAME)
}

fn index_path(dir: &Path, base: u64) -> PathBuf {
    index_dir(dir).join(format!("{base:020}.idx"))
}

/// The names in `seek-index/`, sorted.
fn index_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(index_dir(dir))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// The records of a segment, by the chain, and its seal.
fn chain(path: &Path) -> (Vec<Entry>, u64) {
    let image = std::fs::read(path).unwrap();
    let header = format::decode_segment_header(image[..64].try_into().unwrap()).unwrap();
    let (mut at, mut seq, mut out) = (FIRST_SLOT, header.base_seq, Vec::new());
    while let Ok(format::Slot::Record {
        len, next_offset, ..
    }) = format::read_slot(&image[..], at, seq)
    {
        let crc = &image[at as usize + 4..at as usize + 8];
        out.push(Entry {
            seq,
            offset: at,
            len,
            crc: u32::from_le_bytes(crc.try_into().unwrap()),
        });
        at = next_offset;
        seq += 1;
    }
    (out, at)
}

fn segment_header(path: &Path) -> [u8; 64] {
    std::fs::read(path).unwrap()[..64].try_into().unwrap()
}

/// An index of the segment at `path` with these entries, whatever they are worth, naming this seal.
fn forge(path: &Path, entries: &[Entry], seal: (u64, u64)) -> Vec<u8> {
    let mut encoder = Encoder::with_spacing(4_096);
    for entry in entries {
        encoder.push(*entry);
    }
    encoder
        .finish(&segment_header(path), seal.0, seal.1)
        .unwrap()
}

fn read_up_to(reader: &mut Reader, limit: usize) -> (Vec<OwnedRecord>, Option<String>) {
    let mut records = Vec::new();
    while records.len() < limit {
        match reader.next_owned() {
            Ok(Some(record)) => records.push(record),
            Ok(None) => return (records, Some("end".into())),
            Err(e) => return (records, Some(format!("{e:?}"))),
        }
    }
    (records, None)
}

/// A seek of `reader` gives what a seek of a fresh reader without the option gives: the same result, position, records
/// and errors. Returns whether it used an index.
fn agrees(dir: &Path, reader: &mut Reader, kind: (bool, bool), target: u64, read: usize) -> bool {
    let mut plain = open_with(dir, kind, false);
    let (got, want) = (reader.seek(target), plain.seek(target));
    assert_eq!(
        format!("{got:?}"),
        format!("{want:?}"),
        "{kind:?}: {target}"
    );
    if got.is_err() {
        return false;
    }
    let used = reader.seek_used_index();
    assert_eq!(reader.position(), plain.position());
    let got = read_up_to(reader, read);
    let want = read_up_to(&mut plain, read);
    assert_eq!(got, want, "{kind:?}: seek to {target}, index used: {used}");
    assert_eq!(reader.position(), plain.position());
    if read > 0 {
        assert_eq!(offset(reader), offset(&plain));
    }
    used
}

/// Fresh readers of each kind agree with ones without the option for these targets. Returns how many used an index.
fn all_agree(dir: &Path, targets: &[u64], read: usize) -> usize {
    let mut used = 0;
    for kind in KINDS {
        for &target in targets {
            used += usize::from(agrees(dir, &mut open(dir, kind), kind, target, read));
        }
    }
    used
}

/// (Plan 3) The bound: what a jump skips before the target starts less than one spacing before it.
#[test]
fn a_seek_through_an_index_lands_within_a_block_of_its_target() {
    for len in [
        0,
        64,
        1024,
        100 * 1024,
        4 * SMALL_SEGMENT as usize - 64 - 8 - 40 - 400,
    ] {
        let (dir, _writer) = indexed(len, 2);
        let segs = segments(dir.path());
        for (base, path) in &segs[..2] {
            let (records, _) = chain(path);
            let first = std::fs::read(index_path(dir.path(), *base)).ok().map(|b| {
                codec::decode(&b, &segment_header(path))
                    .unwrap()
                    .entries()
                    .next()
                    .unwrap()
                    .seq
            });
            let step = (records.len() / 40).max(1);
            let targets = records
                .iter()
                .step_by(step)
                .chain(records.last())
                .chain(first.map(|seq| &records[(seq - base) as usize]))
                .chain(first.map(|seq| &records[(seq - base - 1) as usize]));
            for target in targets {
                for kind in KINDS {
                    let mut reader = open(dir.path(), kind);
                    let used = agrees(dir.path(), &mut reader, kind, target.seq, 0);
                    let case = format!("{len} B, {kind:?}, record {}", target.seq);
                    assert_eq!(used, first.is_some_and(|f| f <= target.seq), "{case}");
                    let from = offset(&reader);
                    if used {
                        assert!(from >= SMALL_SEGMENT, "{case}");
                    } else {
                        assert_eq!(from, FIRST_SLOT, "{case}");
                    }
                    assert!(target.offset - from < SMALL_SEGMENT, "{case}: from {from}");
                    agrees(dir.path(), &mut open(dir.path(), kind), kind, target.seq, 3);
                }
            }
        }
    }
}

#[test]
fn every_target_agrees_with_a_scan() {
    let (dir, mut writer) = indexed(200, 3);
    append(&mut writer, 9, 10);
    let next = writer.next_seq();
    let mut targets = Vec::new();
    for (base, path) in &segments(dir.path())[..3] {
        let (records, _) = chain(path);
        targets.push(*base);
        for pair in records.windows(2) {
            if pair[0].offset / SMALL_SEGMENT != pair[1].offset / SMALL_SEGMENT {
                targets.extend([pair[0].seq, pair[1].seq, pair[1].seq + 1]);
            }
        }
        targets.push(records.last().unwrap().seq);
    }
    targets.extend([next - 2, next - 1, next, next + 5, u64::MAX]);
    let used = all_agree(dir.path(), &targets, usize::MAX);
    // Each segment: two of the three around each of the three block edges, and the last record; safe, mapped, visible
    // and durable-only.
    assert_eq!(used, 4 * 3 * (3 * 3 - 1 + 1));
    // Off by default, and off is off.
    for kind in KINDS {
        let mut reader = open_with(dir.path(), kind, false);
        reader.seek(segments(dir.path())[2].0 - 1).unwrap();
        assert!(!reader.seek_used_index());
        assert_eq!(offset(&reader), FIRST_SLOT);
    }
    assert!(!ReaderConfig::default().seek_index);
}

/// A reader that keeps its place: forward seeks from a cursor use an index only where it saves enough, and a landing
/// leaves nothing that a later seek trusts (step 9).
#[test]
fn forward_seeks_use_an_index_only_for_a_distance() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config().segment_size(1 << 20)).unwrap();
    fill(&mut writer, dir.path(), 1, 216, 1);
    writer.checkpoint().unwrap();
    build(dir.path());
    // 256-byte frames: 1,024 records are 256 KiB.
    for kind in KINDS {
        let mut reader = open(dir.path(), kind);
        reader.seek(0).unwrap();
        assert!(!reader.seek_used_index(), "the segment's first record");
        assert_eq!(read_up_to(&mut reader, 300).0.len(), 300);
        let at = offset(&reader);
        reader.seek(300 + 1_023).unwrap();
        assert!(!reader.seek_used_index(), "{kind:?}: close");
        assert_eq!(
            offset(&reader),
            at,
            "{kind:?}: it continues from the cursor"
        );
        reader.seek(300 + 1_024).unwrap();
        assert!(reader.seek_used_index(), "{kind:?}: far");
        assert!(offset(&reader) > at + (256 - 64) * 1024);
        assert_eq!(read_up_to(&mut reader, 1).0[0].seq, 1_324);
        // The cursor came from an index: the next forward seek does not start there, however close.
        let at = offset(&reader);
        reader.seek(1_330).unwrap();
        assert!(reader.seek_used_index());
        assert!(offset(&reader) < at);
        // Without the index, it scans.
        let path = index_path(dir.path(), 0);
        let mut bytes = std::fs::read(&path).unwrap();
        assert_eq!(read_up_to(&mut reader, 1).0[0].seq, 1_330);
        std::fs::remove_file(&path).unwrap();
        reader.seek(1_340).unwrap();
        assert!(!reader.seek_used_index());
        assert_eq!(offset(&reader), FIRST_SLOT);
        assert_eq!(read_up_to(&mut reader, 1).0[0].seq, 1_340);
        // And once it has scanned, the next one continues.
        let at = offset(&reader);
        reader.seek(1_341).unwrap();
        assert_eq!(offset(&reader), at);
        // An entry at or behind the cursor is not used, although the target is far.
        let (records, seal) = chain(&segments(dir.path())[0].1);
        let seal = (seal, records.last().unwrap().seq + 1);
        for (entry, used) in [(1_000, false), (1_500, false), (1_501, true)] {
            let forged = forge(&segments(dir.path())[0].1, &[records[entry]], seal);
            std::fs::write(&path, forged).unwrap();
            let mut reader = open(dir.path(), kind);
            reader.seek(0).unwrap();
            assert_eq!(read_up_to(&mut reader, 1_500).0.len(), 1_500);
            let at = offset(&reader);
            reader.seek(seal.1 - 1).unwrap();
            assert_eq!(reader.seek_used_index(), used, "{kind:?}: entry {entry}");
            assert_eq!(
                offset(&reader),
                if used { records[entry].offset } else { at }
            );
            assert_eq!(read_up_to(&mut reader, 1).0[0].seq, seal.1 - 1);
        }
        std::fs::write(&path, std::mem::take(&mut bytes)).unwrap();
    }
}

/// (Plan 5) Every way the index file can be wrong: the seek gives exactly what it gives without one.
#[test]
fn a_file_that_is_not_the_index_is_not_used() {
    let (dir, _writer) = indexed(64, 2);
    let (other, _other_writer) = indexed(64, 2);
    let segs = segments(dir.path());
    let targets = [segs[0].0 + 700, segs[0].0 + 2_000, segs[1].0 + 2_000];
    let good = std::fs::read(index_path(dir.path(), 0)).unwrap();
    let second = std::fs::read(index_path(dir.path(), segs[1].0)).unwrap();
    assert_eq!(all_agree(dir.path(), &targets, 5), 12);
    let path = index_path(dir.path(), 0);
    let unused = |what: &str| {
        // The second segment's index still works.
        assert_eq!(all_agree(dir.path(), &targets[..2], 5), 0, "{what}");
        assert_eq!(all_agree(dir.path(), &targets[2..], 5), 4, "{what}");
    };
    let put = |bytes: &[u8]| {
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, bytes).unwrap();
    };
    std::fs::remove_file(&path).unwrap();
    unused("missing");
    std::fs::create_dir(&path).unwrap();
    unused("a directory");
    std::fs::remove_dir(&path).unwrap();
    std::os::unix::fs::symlink("nowhere", &path).unwrap();
    unused("a dangling symlink");
    std::fs::remove_file(&path).unwrap();
    let elsewhere = dir.path().join("elsewhere.idx");
    std::fs::write(&elsewhere, &good).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
    unused("a symlink to a good index");
    std::fs::remove_file(&path).unwrap();
    let fifo = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    // SAFETY: a valid C string.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    unused("a FIFO");
    std::fs::remove_file(&path).unwrap();
    let socket = std::os::unix::net::UnixListener::bind(&path);
    if socket.is_ok() {
        unused("a socket");
        std::fs::remove_file(&path).unwrap();
    }
    put(&good);
    std::fs::set_permissions(&path, Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&path).is_err() {
        unused("not readable");
    }
    for mode in [0o664, 0o646, 0o666] {
        std::fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
        unused("writable by others");
    }
    std::fs::set_permissions(&path, Permissions::from_mode(0o444)).unwrap();
    assert_eq!(all_agree(dir.path(), &targets, 5), 12, "read-only");

    put(&[]);
    unused("empty");
    put(&vec![0; good.len()]);
    unused("zeros");
    put(&[good.as_slice(), &[0]].concat());
    unused("one byte longer");
    put(&[good.as_slice(), &[0; 24]].concat());
    unused("one entry longer");
    for len in (0..good.len())
        .step_by(17)
        .chain([good.len() - 24, good.len() - 1])
    {
        put(&good[..len]);
        assert_eq!(all_agree(dir.path(), &targets[1..2], 2), 0, "length {len}");
    }
    for bit in (0..good.len() * 8).step_by(7) {
        let mut bytes = good.clone();
        bytes[bit / 8] ^= 1 << (bit % 8);
        put(&bytes);
        assert_eq!(all_agree(dir.path(), &targets[1..2], 2), 0, "bit {bit}");
    }
    for version in [0_u16, 2, 99] {
        let mut bytes = good.clone();
        bytes[8..10].copy_from_slice(&version.to_le_bytes());
        codec::write_crc(&mut bytes);
        put(&bytes);
        unused("another version");
    }
    put(&second);
    unused("another segment's");
    put(&std::fs::read(index_path(other.path(), 0)).unwrap());
    unused("another journal's, of the same base and size");
    put(&good);
    assert_eq!(all_agree(dir.path(), &targets, 5), 12, "the good one again");

    // `seek-index` itself: a symlink to a good directory, a file, not searchable.
    let moved = dir.path().join("moved");
    std::fs::rename(index_dir(dir.path()), &moved).unwrap();
    std::os::unix::fs::symlink(&moved, index_dir(dir.path())).unwrap();
    assert_eq!(all_agree(dir.path(), &targets, 5), 0, "a symlink");
    std::fs::remove_file(index_dir(dir.path())).unwrap();
    std::fs::write(index_dir(dir.path()), b"").unwrap();
    assert_eq!(all_agree(dir.path(), &targets, 5), 0, "a file");
    std::fs::remove_file(index_dir(dir.path())).unwrap();
    assert_eq!(all_agree(dir.path(), &targets, 5), 0, "no directory");
    std::fs::rename(&moved, index_dir(dir.path())).unwrap();
    std::fs::set_permissions(index_dir(dir.path()), Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(index_dir(dir.path())).is_err() {
        assert_eq!(all_agree(dir.path(), &targets, 5), 0, "mode 0000");
    }
    std::fs::set_permissions(index_dir(dir.path()), Permissions::from_mode(0o755)).unwrap();
    assert_eq!(all_agree(dir.path(), &targets, 5), 12);
}

/// (Plan 5, 8a and 8b) Well-formed indexes whose entries are wrong, and right ones whose segment is damaged.
#[test]
fn an_entry_that_does_not_name_a_record_is_not_used() {
    let (dir, _writer) = indexed(64, 1);
    let path = segments(dir.path())[0].1.clone();
    let (records, seal) = chain(&path);
    let seal = (seal, records.last().unwrap().seq + 1);
    let target = 2_000;
    let right = records[1_900];
    let put = |entry: Entry| {
        std::fs::write(index_path(dir.path(), 0), forge(&path, &[entry], seal)).unwrap();
    };
    put(right);
    assert_eq!(all_agree(dir.path(), &[target], 5), 4, "a right one");
    for (what, wrong) in [
        (
            "seq",
            Entry {
                seq: 1_899,
                ..right
            },
        ),
        (
            "seq",
            Entry {
                seq: 1_901,
                ..right
            },
        ),
        (
            "offset",
            Entry {
                offset: right.offset + 8,
                ..right
            },
        ),
        (
            "offset",
            Entry {
                offset: right.offset - 104,
                ..right
            },
        ),
        (
            "len",
            Entry {
                len: right.len - 1,
                ..right
            },
        ),
        (
            "len",
            Entry {
                len: right.len + 1,
                ..right
            },
        ),
        (
            "crc",
            Entry {
                crc: right.crc ^ 1,
                ..right
            },
        ),
        (
            "inside a payload",
            Entry {
                offset: right.offset + 48,
                ..right
            },
        ),
    ] {
        put(wrong);
        assert_eq!(all_agree(dir.path(), &[target], 5), 0, "{what}");
    }
    // The seal: another offset, another number.
    for wrong in [
        (seal.0 - 8, seal.1),
        (seal.0 + 8, seal.1),
        (seal.0, seal.1 + 1),
    ] {
        std::fs::write(index_path(dir.path(), 0), forge(&path, &[right], wrong)).unwrap();
        assert_eq!(all_agree(dir.path(), &[target], 5), 0, "seal {wrong:?}");
    }
    // 11. The record it names is damaged (the header, the payload): the result is the scan's, an error.
    put(right);
    let raw = open_raw(&path);
    for at in [8, 20, 60] {
        let mut byte = [0];
        raw.read_exact_at(&mut byte, right.offset + at).unwrap();
        raw.write_all_at(&[byte[0] ^ 0xFF], right.offset + at)
            .unwrap();
        assert_eq!(all_agree(dir.path(), &[target], 5), 0, "damaged at {at}");
        raw.write_all_at(&byte, right.offset + at).unwrap();
    }
    // The target is.
    let mut byte = [0];
    raw.read_exact_at(&mut byte, records[2_000].offset + 50)
        .unwrap();
    raw.write_all_at(&[byte[0] ^ 0xFF], records[2_000].offset + 50)
        .unwrap();
    assert_eq!(
        all_agree(dir.path(), &[target, target - 3], 5),
        8,
        "damaged target"
    );
}

/// (Plan 11) Damage before the landing record: found without the option, by `verify` and by replay, and not by the jump.
#[test]
fn damage_before_the_landing_record_is_stepped_over() {
    for zeroed in [false, true] {
        let (dir, _writer) = indexed(64, 1);
        let path = segments(dir.path())[0].1.clone();
        let (records, _) = chain(&path);
        let at = records[100].offset;
        let damage: &[u8] = if zeroed { &[0; 4] } else { &[0xEE; 4] };
        open_raw(&path)
            .write_all_at(damage, if zeroed { at } else { at + 50 })
            .unwrap();
        for kind in KINDS {
            let mut reader = open(dir.path(), kind);
            reader.seek(2_000).unwrap();
            assert!(reader.seek_used_index());
            let (got, _) = read_up_to(&mut reader, 5);
            assert!(got.iter().map(|r| r.seq).eq(2_000..2_005));
            let mut plain = open_with(dir.path(), kind, false);
            plain.seek(2_000).unwrap();
            let (got, end) = read_up_to(&mut plain, 5);
            assert!(got.is_empty() && end.is_some(), "{kind:?}: {end:?}");
            // A seek that no entry serves scans, with the option too.
            let mut reader = open(dir.path(), kind);
            reader.seek(120).unwrap();
            assert!(!reader.seek_used_index());
            assert!(read_up_to(&mut reader, 5).0.is_empty());
        }
        assert!(
            !tickertape::inspect::verify(dir.path())
                .unwrap()
                .problems
                .is_empty()
        );
        // A damaged segment gets no new index.
        seek_index::remove(dir.path()).unwrap();
        assert_eq!(build(dir.path())[0], IndexState::NotSealed);
    }
}

/// (Plan 2) Any bytes at all as the index: what a seek does is what it does without one.
#[test]
fn any_index_file_gives_the_same_results() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    // Payloads that hold no frames.
    while segments(dir.path()).len() < 3 {
        let len = (writer.next_seq() as usize * 37) % 900;
        append(&mut writer, 0xAB, len);
    }
    writer.checkpoint().unwrap();
    build(dir.path());
    let segs = segments(dir.path());
    let good = std::fs::read(index_path(dir.path(), 0)).unwrap();
    let last = segs[1].0 - 1;
    let used = AtomicU64::new(0);
    let mut runner = TestRunner::new(Config {
        cases: 400,
        failure_persistence: None,
        ..Config::default()
    });
    let bytes = prop_oneof![
        1 => proptest::collection::vec(any::<u8>(), 0..600),
        // Fields set to anything, or a little off, with a checksum that fits.
        6 => (proptest::collection::vec((0_usize..good.len() / 4, any::<u32>(), any::<bool>()), 1..4), Just(good.clone()))
            .prop_map(|(fields, mut bytes)| {
                for (at, value, near) in fields {
                    let at = at * 4;
                    if at + 4 <= bytes.len() {
                        let old = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
                        let new = if near {
                            old.wrapping_add(value % 400).wrapping_sub(200)
                        } else {
                            value
                        };
                        bytes[at..at + 4].copy_from_slice(&new.to_le_bytes());
                    }
                }
                codec::write_crc(&mut bytes);
                bytes
            }),
        1 => Just(good.clone()),
    ];
    runner
        .run(&(bytes, 0..=last, 0_usize..4), |(bytes, target, kind)| {
            std::fs::write(index_path(dir.path(), 0), &bytes).unwrap();
            let kind = KINDS[kind];
            if agrees(dir.path(), &mut open(dir.path(), kind), kind, target, 4) {
                used.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        })
        .unwrap();
    assert!(used.into_inner() > 10);
}

// ---- Stale indexes and the removal at the writer's open ----

/// How recovery changes the first segment in [`Stale`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Repair {
    /// Commit words emptied from a record to the end, the seal and the next segment gone: zeroed in place.
    InPlace,
    /// A record's payload damaged: the file is replaced, the next segments are quarantined.
    Replaced,
}

/// A journal without a marker whose first segment has an index, damaged at record `at` for the next open to repair.
struct Stale {
    dir: tempfile::TempDir,
    at: u64,
    index: Vec<u8>,
}

impl Stale {
    const LEN: usize = 64;

    fn new(repair: Repair, at: u64) -> Stale {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), config()).unwrap();
        fill(&mut writer, dir.path(), 1, Stale::LEN, 1);
        drop(writer);
        assert_eq!(
            build(dir.path()),
            [IndexState::Built, IndexState::NotSealed]
        );
        let index = std::fs::read(index_path(dir.path(), 0)).unwrap();
        let stale = Stale { dir, at, index };
        stale.damage(repair);
        stale
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The damage itself.
    fn damage(&self, repair: Repair) {
        let segs = segments(self.path());
        let path = &segs[0].1;
        let (records, seal) = chain(path);
        let raw = open_raw(path);
        let at = records[self.at as usize].offset;
        match repair {
            Repair::InPlace => {
                for (_, later) in &segs[1..] {
                    std::fs::remove_file(later).unwrap();
                }
                raw.write_all_at(&[0; 8], seal).unwrap();
                zero_commit_words_from(self.path(), self.at);
            }
            Repair::Replaced => raw.write_all_at(&[0xEE], at + 50).unwrap(),
        }
    }

    /// Opens the writer with the removal of indexes disabled, as a 1.0.0 writer, which does not know of them, would.
    fn open_without_removal(&self) -> Writer {
        fault::clear();
        for _ in 0..2 {
            fault::fail_next(Point::SeekIndexList, libc::EIO);
        }
        let writer = Writer::open(self.path(), config()).unwrap();
        assert_eq!(
            (
                writer.recovery_report().seek_indexes_removed,
                writer.recovery_report().seek_index_errors
            ),
            (0, 2)
        );
        assert_eq!(writer.next_seq(), self.at);
        assert_eq!(
            std::fs::read(index_path(self.path(), 0)).unwrap(),
            self.index
        );
        writer
    }

    /// Targets around the place of the repair, in every block, and at the end.
    fn targets(&self) -> Vec<u64> {
        let (records, _) = chain(&segments(self.path())[0].1);
        let last = records.last().unwrap().seq;
        let mut out: Vec<u64> = (0..24).map(|i| last * i / 23).collect();
        out.extend([self.at - 1, self.at, self.at + 1, last, last + 1]);
        out
    }
}

/// (Plan 6) An index that the writer did not remove, of every history a repair leaves behind: every seek gives what a scan
/// gives, never a removed record.
#[test]
fn a_stale_index_never_gives_a_removed_record() {
    for repair in [Repair::InPlace, Repair::Replaced] {
        // The repair starts in the first block, in the middle, or near the end.
        for at in [40, 1_300, 2_450] {
            // Equal records (the new history then ends at the same offset and number), shorter, longer.
            for len in [Stale::LEN, 40, 300] {
                let stale = Stale::new(repair, at);
                let mut writer = stale.open_without_removal();
                let dir = stale.path();
                assert_eq!(segments(dir).len(), 1);
                // Not sealed yet.
                for _ in 0..30 {
                    append(&mut writer, 2, len);
                }
                all_agree(dir, &stale.targets(), 3);
                fill(&mut writer, dir, 2, len, 1);
                let used = all_agree(dir, &stale.targets(), 3);
                if len != Stale::LEN {
                    assert_eq!(used, 0, "another seal: {repair:?}, {at}, {len}");
                } else {
                    // Entries of records the repair kept still serve; nothing is trusted by the others.
                    assert_eq!(used > 0, at > 700, "{repair:?}, {at}");
                }
                // Durable-only and mapped readers use none until a marker covers the segment; then the same holds.
                writer.checkpoint().unwrap();
                all_agree(dir, &stale.targets(), 3);
                // The builder sees at the last entry that it is stale.
                // (If the repair came after the last indexed record and the new history ends alike, it is right.)
                let right = len == Stale::LEN && at == 2_450;
                let (before, built) = if right {
                    (IndexState::Valid, IndexState::Valid)
                } else {
                    (IndexState::Ignored, IndexState::Built)
                };
                assert_eq!(check(dir)[0], before);
                assert_eq!(build(dir)[0], built);
                assert_eq!(check(dir)[0], IndexState::Valid);
                assert!(all_agree(dir, &stale.targets(), 3) > 10);
            }
        }
    }
}

/// (Plan 6) A segment whose header recovery wrote again, and one quarantined and created anew.
#[test]
fn an_index_of_a_segment_made_anew_is_not_used() {
    for zeroed in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), config()).unwrap();
        fill(&mut writer, dir.path(), 1, 64, 2);
        drop(writer);
        assert_eq!(build(dir.path())[1], IndexState::Built);
        let segs = segments(dir.path());
        let index = index_path(dir.path(), segs[1].0);
        let bytes = std::fs::read(&index).unwrap();
        let raw = open_raw(&segs[1].1);
        if zeroed {
            raw.write_all_at(&[0; 64], 0).unwrap();
        } else {
            std::fs::remove_file(&segs[2].1).unwrap();
            raw.write_all_at(&[7], 20).unwrap();
        }
        fault::clear();
        for _ in 0..2 {
            fault::fail_next(Point::SeekIndexList, libc::EIO);
        }
        let mut writer = Writer::open(dir.path(), config()).unwrap();
        assert!(!writer.recovery_report().actions.is_empty());
        assert_eq!(std::fs::read(&index).unwrap(), bytes);
        fill(
            &mut writer,
            dir.path(),
            if zeroed { 1 } else { 2 },
            64,
            if zeroed { 3 } else { 2 },
        );
        let (records, _) = chain(&segs[1].1);
        let targets: Vec<u64> = records.iter().step_by(200).map(|r| r.seq).collect();
        assert_eq!(all_agree(dir.path(), &targets, 3), 0);
        writer.checkpoint().unwrap();
        assert_eq!(all_agree(dir.path(), &targets, 3), 0);
        assert_eq!(check(dir.path())[1], IndexState::Ignored);
    }
}

/// (Plan 6) A reader that has the old file open, and an index built from the file that replaced it while it seeks (step 5).
#[test]
fn an_index_of_the_replaced_file_is_not_applied_to_the_old_one() {
    // Where recovery replaces the file: before the index is read, and after.
    for point in [Point::ReaderSeekIndexMarker, Point::ReaderSeekIndexLoaded] {
        for repair in [Repair::Replaced, Repair::InPlace] {
            // The damage is done by the hook, to a journal that is whole when the seek begins.
            let dir = tempfile::tempdir().unwrap();
            let mut writer = Writer::open(dir.path(), config()).unwrap();
            fill(&mut writer, dir.path(), 1, Stale::LEN, 1);
            drop(writer);
            build(dir.path());
            let stale = Rc::new(Stale {
                index: Vec::new(),
                at: 2_400,
                dir,
            });
            let target = 2_300;
            let mut plain = open_with(stale.path(), (false, false), false);
            let mut reader = open(stale.path(), (false, false));
            plain.seek(target).unwrap();
            let hook = Rc::clone(&stale);
            let ran = Rc::new(Cell::new(false));
            let done = Rc::clone(&ran);
            fault::clear();
            fault::run_at(point, 0, move || {
                hook.damage(repair);
                let mut writer = Writer::open(hook.path(), config()).unwrap();
                fill(&mut writer, hook.path(), 3, Stale::LEN, 1);
                drop(writer);
                build(hook.path());
                done.set(true);
            });
            reader.seek(target).unwrap();
            assert!(ran.get());
            let used = reader.seek_used_index();
            let case = format!("{point:?}, {repair:?}, {target}, used {used}");
            if repair == Repair::Replaced {
                assert!(!used, "{case}");
                // The old file, like any reader that has it open.
                let (got, want) = (
                    read_up_to(&mut reader, usize::MAX),
                    read_up_to(&mut plain, usize::MAX),
                );
                assert_eq!(got, want, "{case}");
                assert!(want.1.unwrap().contains("Discarded"));
            } else {
                // The same file, now with the new history.
                let (got, end) = read_up_to(&mut reader, usize::MAX);
                assert_eq!(end.as_deref(), Some("end"), "{case}");
                assert!(
                    got.iter()
                        .all(|r| r.payload[0] == if r.seq < 2_400 { 1 } else { 3 })
                );
                assert_eq!(got[0].seq, target);
            }
            // A fresh reader uses the index of the file that is there now.
            agrees(
                stale.path(),
                &mut open(stale.path(), (false, false)),
                (false, false),
                target,
                3,
            );
        }
    }
}

/// (Plan 6) A forward seek that may continue from the cursor, whose index fails at each step, across a repair in place under a
/// stale read buffer: it continues from where it was, with an empty buffer.
#[test]
fn a_failed_index_leaves_the_cursor_and_an_empty_buffer() {
    for (failure, kind) in (0..6).flat_map(|f| [(f, (false, false)), (f, (true, false))]) {
        let dir = tempfile::tempdir().unwrap();
        let big = config().segment_size(1 << 20);
        let mut writer = Writer::open(dir.path(), big.clone()).unwrap();
        // 4,040-byte frames: a read buffer holds 16 of them.
        for _ in 0..102 {
            append(&mut writer, 1, 4_000);
        }
        writer.checkpoint().unwrap();
        for _ in 0..30 {
            append(&mut writer, 1, 4_000);
        }
        drop(writer);
        let mut reader = open(dir.path(), kind);
        reader.seek(0).unwrap();
        assert_eq!(read_up_to(&mut reader, 100).0.len(), 100);
        let at = offset(&reader);
        // Records 104 and later, some of which the safe reader has buffered, are removed in place.
        zero_commit_words_from(dir.path(), 104);
        let mut writer = Writer::open(dir.path(), big).unwrap();
        assert_eq!(writer.next_seq(), 104);
        fill(&mut writer, dir.path(), 2, 3_000, 1);
        writer.checkpoint().unwrap();
        build(dir.path());
        let path = segments(dir.path())[0].1.clone();
        let (records, seal) = chain(&path);
        let seal = (seal, records.last().unwrap().seq + 1);
        let entry = records[200];
        let put = |entry: Entry, seal| {
            std::fs::write(index_path(dir.path(), 0), forge(&path, &[entry], seal)).unwrap();
        };
        match failure {
            0 => std::fs::remove_file(index_path(dir.path(), 0)).unwrap(),
            1 => std::fs::write(index_path(dir.path(), 0), b"junk").unwrap(),
            2 => put(entry, (seal.0, seal.1 + 1)),
            // 8a, then 8b, then one that works.
            3 => put(
                Entry {
                    crc: entry.crc ^ 1,
                    ..entry
                },
                seal,
            ),
            4 => put(
                Entry {
                    seq: entry.seq - 1,
                    ..entry
                },
                seal,
            ),
            _ => put(entry, seal),
        }
        reader.seek(230).unwrap();
        assert_eq!(
            reader.seek_used_index(),
            failure == 5,
            "{failure}, {kind:?}"
        );
        let expected = if failure == 5 { entry.offset } else { at };
        assert_eq!(offset(&reader), expected, "{failure}, {kind:?}");
        let (got, end) = read_up_to(&mut reader, usize::MAX);
        assert_eq!(end.as_deref(), Some("end"), "{failure}, {kind:?}");
        assert!(got.iter().map(|r| r.seq).eq(230..writer.next_seq()));
        assert!(got.iter().all(|r| r.payload[0] == 2));
    }
}

/// (Plan 7) The open of an index-aware writer removes the indexes of the recovery domain, before it changes anything, and no
/// others, and reports how many.
#[test]
fn the_writers_open_removes_the_indexes_of_the_recovery_domain() {
    for policy in [RecoveryPolicy::Quarantine, RecoveryPolicy::Fail] {
        for lost in ["nothing", "newest slot", "both slots"] {
            let dir = tempfile::tempdir().unwrap();
            let mut writer = Writer::open(dir.path(), config()).unwrap();
            fill(&mut writer, dir.path(), 1, 64, 2);
            writer.checkpoint().unwrap();
            let marker = dir.path().join("checkpoint");
            let older = std::fs::read(&marker).unwrap();
            fill(&mut writer, dir.path(), 1, 64, 4);
            writer.checkpoint().unwrap();
            drop(writer);
            let segs = segments(dir.path());
            build(dir.path());
            assert_eq!(index_names(dir.path()).len(), 4);
            match lost {
                "newest slot" => std::fs::write(&marker, &older).unwrap(),
                "both slots" => std::fs::write(&marker, vec![0; older.len()]).unwrap(),
                _ => {}
            }
            // Damage in segment 3, which an intact marker keeps out of the recovery domain.
            let (records, _) = chain(&segs[3].1);
            let damaged = &records[1_000];
            open_raw(&segs[3].1)
                .write_all_at(&[0xEE], damaged.offset + 50)
                .unwrap();
            let removed = Rc::new(Cell::new(None));
            let (seen, at) = (Rc::clone(&removed), dir.path().to_path_buf());
            let (path, before) = (segs[3].1.clone(), std::fs::read(&segs[3].1).unwrap());
            fault::clear();
            fault::run_at(Point::SeekIndexRemoved, 0, move || {
                assert_eq!(std::fs::read(&path).unwrap(), before, "nothing changed yet");
                seen.set(Some(index_names(&at).len()));
            });
            let opened = Writer::open(dir.path(), config().recovery(policy));
            let left = match lost {
                "nothing" => 4,
                "newest slot" => 2,
                _ => 0,
            };
            assert_eq!(removed.get(), Some(left), "{lost}, {policy:?}");
            if lost != "nothing" && policy == RecoveryPolicy::Fail {
                assert!(matches!(opened, Err(Error::Corrupt { .. })), "{lost}");
            } else {
                let report = opened.unwrap().recovery_report().clone();
                assert_eq!(report.actions.is_empty(), lost == "nothing");
                assert_eq!(report.seek_indexes_removed, 4 - left as u64);
                assert_eq!(report.seek_index_errors, 0);
            }
            assert_eq!(index_names(dir.path()).len(), left);
            let (first, _) = chain(&segs[0].1);
            all_agree(
                dir.path(),
                &[first[2_000].seq, damaged.seq - 1, damaged.seq + 5],
                3,
            );
        }
    }
}

/// (Plan 7) What the open removes goes by name.
#[test]
fn the_removal_goes_by_name() {
    let (dir, writer) = indexed(64, 2);
    drop(writer);
    let segs = segments(dir.path());
    let names = [
        // Removed: temporary files of any segment; an index, also of a later kind, of the marker's segment or later.
        format!(".{:020}.77.1.idx-tmp", 0),
        format!(".{:020}.77.1.idx-tmp", 999_999),
        format!("{:020}.idx", segs[2].0),
        format!("{:020}.idx2", segs[2].0),
        format!("{:020}.idx.sha", segs[2].0 + 7),
        // Kept: others.
        format!("{:020}.idx2", segs[1].0),
        format!("{:020}.idX", segs[2].0 + 1),
        "foo.idx-tmp".into(),
        "notes.txt".into(),
        format!(".{:020}.77.idx-tmp", 0),
        format!("{:020}.tape", segs[2].0),
    ];
    let dir_path = index_dir(dir.path());
    for name in &names {
        std::fs::write(dir_path.join(name), b"x").unwrap();
    }
    let writer = Writer::open(dir.path(), config()).unwrap();
    assert_eq!(writer.recovery_report().seek_indexes_removed, 5);
    assert_eq!(writer.recovery_report().actions, []);
    drop(writer);
    let mut left: Vec<String> = names[5..].to_vec();
    left.extend([0, segs[1].0].map(|base| format!("{base:020}.idx")));
    left.sort();
    assert_eq!(index_names(dir.path()), left);

    // A dangling marker refuses the open and leaves every index; so does a marker whose offset is not one.
    std::fs::write(dir_path.join(&names[0]), b"x").unwrap();
    let moved = dir.path().join("moved");
    std::fs::rename(&segs[2].1, &moved).unwrap();
    assert!(matches!(
        Writer::open(dir.path(), config()),
        Err(Error::Corrupt { .. })
    ));
    assert_eq!(index_names(dir.path()).len(), left.len() + 1);
    assert!(matches!(
        seek_index::build(dir.path(), false),
        Err(Error::Corrupt { .. })
    ));
    assert!(matches!(
        seek_index::check(dir.path()),
        Err(Error::Corrupt { .. })
    ));
    std::fs::rename(&moved, &segs[2].1).unwrap();

    // Segments that are gone: their indexes go at the next open (here removed by a program that knows of none).
    std::fs::remove_file(&segs[0].1).unwrap();
    let writer = Writer::open(dir.path(), config()).unwrap();
    // The temporary one and segment 0's.
    assert_eq!(writer.recovery_report().seek_indexes_removed, 2);
    drop(writer);
    assert!(!index_path(dir.path(), 0).exists());
    assert!(index_path(dir.path(), segs[1].0).exists());
    // A journal wiped and created again loses them all.
    for (_, path) in &segs[1..] {
        std::fs::remove_file(path).unwrap();
    }
    std::fs::remove_file(dir.path().join("checkpoint")).unwrap();
    let writer = Writer::open(dir.path(), config()).unwrap();
    assert_eq!(writer.recovery_report().seek_indexes_removed, 2);
    drop(writer);
    // Segment 0's went before; segment 1's and its `.idx2` now.
    assert_eq!(index_names(dir.path()).len(), left.len() - 3);

    // `remove` takes every recognised name, and leaves the rest and the directory.
    std::fs::write(index_path(dir.path(), 5), b"x").unwrap();
    std::fs::write(dir_path.join(&names[0]), b"x").unwrap();
    std::fs::write(dir_path.join(&names[3]), b"x").unwrap();
    assert_eq!(seek_index::remove(dir.path()).unwrap(), 3);
    assert_eq!(seek_index::remove(dir.path()).unwrap(), 0);
    assert!(dir_path.join("notes.txt").exists());
}

/// (Plan 7) The second pass sees what a listing missed: a name created during the first, and both names of a rename.
#[test]
fn the_second_pass_sees_what_the_first_missed() {
    for omitted in [false, true] {
        let stale = Stale::new(Repair::InPlace, 1_000);
        let index = index_path(stale.path(), 0);
        fault::clear();
        if omitted {
            // A listing during which the temporary file was renamed may show neither name.
            fault::next_listing_omits([OsString::from(index.file_name().unwrap())]);
        } else {
            std::fs::remove_file(&index).unwrap();
            let (index, bytes) = (index.clone(), stale.index.clone());
            fault::run_at(Point::SeekIndexListed, 0, move || {
                std::fs::write(index, bytes).unwrap();
            });
        }
        let writer = Writer::open(stale.path(), config()).unwrap();
        assert_eq!(writer.recovery_report().seek_indexes_removed, 1);
        assert!(!index.exists());
    }
}

/// (Plan 7) Best effort: what cannot be listed, unlinked or synced is counted and never stops the open; what stays is checked
/// by value.
#[test]
fn a_removal_that_fails_is_counted_and_does_not_stop_the_open() {
    let root = {
        // SAFETY: no preconditions.
        unsafe { libc::geteuid() == 0 }
    };
    for case in 0..9 {
        let stale = Stale::new(Repair::InPlace, 1_300);
        let dir = index_dir(stale.path());
        let mode = |m| std::fs::set_permissions(&dir, Permissions::from_mode(m)).unwrap();
        fault::clear();
        // (failures, names removed, the index stays)
        let (errors, removed, stays) = match case {
            0 => (0, 1, false),
            1 => {
                mode(0o555);
                (1, 0, true)
            }
            2 => {
                mode(0o000);
                (1, 0, true)
            }
            3 => {
                // Directories with names of an index and of a temporary file.
                std::fs::create_dir(dir.join(format!("{:020}.idx", 5))).unwrap();
                std::fs::create_dir(dir.join(format!(".{:020}.1.1.idx-tmp", 5))).unwrap();
                (2, 1, false)
            }
            4 => {
                std::fs::remove_dir_all(&dir).unwrap();
                std::fs::write(&dir, b"a beacon, perhaps").unwrap();
                (0, 0, false)
            }
            5 => {
                // One failed attempt: the second pass removes it.
                fault::fail_next(Point::SeekIndexUnlink, libc::EIO);
                (0, 1, false)
            }
            6 => {
                fault::fail_next(Point::SeekIndexUnlink, libc::EIO);
                fault::fail_next(Point::SeekIndexUnlink, libc::EIO);
                (1, 0, true)
            }
            7 => {
                fault::fail_next(Point::SeekIndexList, libc::EIO);
                (1, 1, false)
            }
            _ => {
                fault::fail_next(Point::SeekIndexSync, libc::EIO);
                (1, 1, false)
            }
        };
        if root && (1..=2).contains(&case) {
            continue;
        }
        let mut writer = Writer::open(stale.path(), config()).unwrap();
        let report = writer.recovery_report();
        assert_eq!(report.seek_index_errors, errors, "case {case}");
        assert_eq!(report.seek_indexes_removed, removed, "case {case}");
        assert_eq!(report.next_seq, 1_300);
        if case != 4 {
            mode(0o755);
            assert_eq!(index_path(stale.path(), 0).exists(), stays, "case {case}");
        } else {
            assert!(dir.is_file());
            assert!(seek_index::build(stale.path(), false).is_err());
            assert!(seek_index::remove(stale.path()).is_err());
        }
        fill(&mut writer, stale.path(), 2, 64, 1);
        all_agree(stale.path(), &stale.targets(), 3);
    }
}

/// (Plan 5) `seek-index` as a symlink to another journal's: nothing is read, removed or built there.
#[test]
fn a_symlinked_directory_is_never_followed() {
    let (other, _writer) = indexed(64, 2);
    let names = index_names(other.path());
    let stale = Stale::new(Repair::InPlace, 1_300);
    std::fs::remove_dir_all(index_dir(stale.path())).unwrap();
    std::os::unix::fs::symlink(index_dir(other.path()), index_dir(stale.path())).unwrap();
    let mut writer = Writer::open(stale.path(), config()).unwrap();
    assert_eq!(writer.recovery_report().seek_indexes_removed, 0);
    fill(&mut writer, stale.path(), 2, 64, 1);
    writer.checkpoint().unwrap();
    assert!(seek_index::build(stale.path(), true).is_err());
    assert!(seek_index::remove(stale.path()).is_err());
    assert_eq!(all_agree(stale.path(), &[2_000], 3), 0);
    let policy = Policy::default().max_segments(1);
    let plan = retention::plan(stale.path(), &policy).unwrap();
    assert_eq!(
        retention::apply(stale.path(), &plan, &Action::Delete)
            .unwrap()
            .removed_segments,
        1
    );
    assert_eq!(index_names(other.path()), names);
    assert_eq!(check(other.path())[0], IndexState::Valid);
}

// ---- Durable-only and mapped readers, T6 and T7 ----

/// (Plan 8) Segments the marker does not cover: durable-only and mapped readers use no index there, whatever it holds; a safe
/// visible reader may. Without a marker (an archive) only the safe visible one does.
#[test]
fn durable_only_and_mapped_readers_need_a_covering_marker() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    fill(&mut writer, dir.path(), 1, 64, 1);
    writer.checkpoint().unwrap();
    let marker = dir.path().join("checkpoint");
    let covers_one = std::fs::read(&marker).unwrap();
    fill(&mut writer, dir.path(), 1, 64, 3);
    writer.checkpoint().unwrap();
    let covers_three = std::fs::read(&marker).unwrap();
    build(dir.path());
    drop(writer);
    let segs = segments(dir.path());
    for (bytes, covered) in [(&covers_three[..], 3), (&covers_one, 1), (&[0; 131_072], 0)] {
        std::fs::write(&marker, bytes).unwrap();
        for (i, (base, _)) in segs[..3].iter().enumerate() {
            for kind in KINDS {
                let mut reader = open(dir.path(), kind);
                let used = agrees(dir.path(), &mut reader, kind, base + 2_000, 4);
                assert_eq!(used, i < covered || kind == (false, false), "{i}: {kind:?}");
            }
        }
    }
    std::fs::remove_file(&marker).unwrap();
    for kind in KINDS {
        let mut reader = open(dir.path(), kind);
        assert_eq!(
            agrees(dir.path(), &mut reader, kind, 2_000, 4),
            kind == (false, false)
        );
    }
}

/// (Plan 8) The marker is lost between the seek and the read: nothing is covered, so a durable-only reader that landed through
/// an index waits; a visible one reads on.
#[test]
fn a_durable_only_reader_waits_when_the_marker_is_lost_after_landing() {
    let (dir, writer) = indexed(64, 1);
    drop(writer);
    let mut readers = KINDS.map(|kind| (kind, open(dir.path(), kind)));
    for (_, reader) in &mut readers {
        reader.seek(2_000).unwrap();
        assert!(reader.seek_used_index());
    }
    let marker = dir.path().join("checkpoint");
    let bytes = std::fs::read(&marker).unwrap();
    std::fs::remove_file(&marker).unwrap();
    for (kind, reader) in &mut readers {
        assert_eq!(
            reader.try_next().unwrap().map(|r| r.seq),
            (!kind.1).then_some(2_000)
        );
    }
    std::fs::write(&marker, bytes).unwrap();
    for (kind, reader) in &mut readers {
        let next = if kind.1 { 2_000 } else { 2_001 };
        assert_eq!(reader.try_next().unwrap().unwrap().seq, next);
    }
}

/// (Plan 8) A repair between a durable-only reader's marker read and its landing, by a recovery from a lost marker, followed by
/// a new checkpoint past the segment and a new index: it lands on the current chain, or scans.
#[test]
fn a_repair_while_a_seek_uses_an_index() {
    for (point, repair, kind) in [
        Point::ReaderSeekIndexMarker,
        Point::ReaderSeekIndexLoaded,
        Point::ReaderSeekIndexLanding,
    ]
    .into_iter()
    .flat_map(|p| [Repair::InPlace, Repair::Replaced].map(|r| (p, r)))
    .flat_map(|(p, r)| KINDS.map(|k| (p, r, k)))
    {
        // A mapped reader must not have the file changed under it: in the process of the writer, that would be a race.
        if kind.0 && repair == Repair::InPlace {
            continue;
        }
        for (len, target) in [(64, 2_350), (64, 2_450), (40, 2_450)] {
            let (dir, writer) = indexed(64, 1);
            drop(writer);
            let stale = Stale {
                index: Vec::new(),
                at: 2_400,
                dir,
            };
            let stale = Rc::new(stale);
            let mut reader = open(stale.path(), kind);
            let hook = Rc::clone(&stale);
            fault::clear();
            fault::run_at(point, 0, move || {
                std::fs::remove_file(hook.path().join("checkpoint")).unwrap();
                hook.damage(repair);
                let mut writer = Writer::open(hook.path(), config()).unwrap();
                assert_eq!(writer.next_seq(), 2_400);
                fill(&mut writer, hook.path(), 2, len, 1);
                writer.checkpoint().unwrap();
                build(hook.path());
            });
            reader.seek(target).unwrap();
            let used = reader.seek_used_index();
            let case = format!("{point:?}, {repair:?}, {kind:?}, {len}, {target}, used {used}");
            let (got, end) = read_up_to(&mut reader, usize::MAX);
            let end = end.unwrap();
            if repair == Repair::Replaced {
                // It holds the file it opened: a visible reader reads on there until it notices, as any would; a
                // durable-only one notices before it applies the marker.
                assert!(end.contains("Discarded"), "{case}: {end}");
                assert!(got.iter().all(|r| r.payload[0] == 1), "{case}");
                assert!(!kind.1 || got.is_empty(), "{case}");
                assert!(!used || point == Point::ReaderSeekIndexLanding, "{case}");
            } else {
                assert_eq!(end, "end", "{case}");
                assert!(
                    got.iter()
                        .map(|r| r.seq)
                        .eq(target..target + got.len() as u64)
                );
                assert!(
                    got.iter()
                        .all(|r| r.payload[0] == if r.seq < 2_400 { 1 } else { 2 }),
                    "{case}"
                );
            }
            agrees(stale.path(), &mut reader, kind, target, 10);
        }
    }
}

/// (Plan 9) Documents T6: a payload that embeds a frame with its own record's number, and an index that points at it. With the
/// option on the embedded frame is returned, and nothing but `--check` can tell; with it off, it never is.
#[test]
fn a_forged_index_can_return_a_frame_from_a_payload() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    for _ in 0..1_000 {
        append(&mut writer, 1, 64);
    }
    // Record 1,000: 48 bytes of payload, then a frame that ends where the record ends.
    let inner = 136_u32;
    let mut payload = vec![0; 48 + inner as usize];
    let header = format::RecordHeader {
        seq: 1_000,
        schema_id: 66,
        venue_ts_ns: 0,
        local_ts_ns: 0,
    };
    format::encode_record_body(&header, &[66; 96], &mut payload[52..]);
    payload[48..52].copy_from_slice(&inner.to_le_bytes());
    let crc = u32::from_le_bytes(payload[52..56].try_into().unwrap());
    writer
        .append(&NewRecord {
            schema_id: 7,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: &payload,
        })
        .unwrap();
    fill(&mut writer, dir.path(), 1, 64, 1);
    writer.checkpoint().unwrap();
    build(dir.path());
    let path = segments(dir.path())[0].1.clone();
    let (records, seal) = chain(&path);
    let outer = records[1_000];
    let embedded = Entry {
        seq: 1_000,
        offset: outer.offset + 40 + 48,
        len: inner,
        crc,
    };
    assert_eq!(
        embedded.offset + format::align8(u64::from(inner)),
        records[1_001].offset
    );
    let index = forge(&path, &[embedded], (seal, records.last().unwrap().seq + 1));
    std::fs::write(index_path(dir.path(), 0), index).unwrap();
    for kind in KINDS {
        let mut reader = open(dir.path(), kind);
        reader.seek(1_000).unwrap();
        assert!(reader.seek_used_index());
        let (got, _) = read_up_to(&mut reader, 3);
        assert_eq!(
            got.iter().map(|r| (r.seq, r.schema_id)).collect::<Vec<_>>(),
            [(1_000, 66), (1_001, 1), (1_002, 1)]
        );
        let mut plain = open_with(dir.path(), kind, false);
        plain.seek(1_000).unwrap();
        assert_eq!(plain.try_next().unwrap().unwrap().schema_id, 7);
    }
    assert_eq!(
        tickertape::inspect::verify(dir.path()).unwrap().problems,
        []
    );
    // The builder's quick check does not see it; `check` does, and `rebuild` repairs it.
    assert_eq!(build(dir.path())[0], IndexState::Valid);
    let report = seek_index::check(dir.path()).unwrap();
    assert_eq!(report.segments[0].state, IndexState::Wrong);
    assert!(report.segments[0].reason.as_ref().unwrap().contains("1000"));
    let report = seek_index::build(dir.path(), true).unwrap();
    assert_eq!(report.segments[0].state, IndexState::Built);
    assert_eq!(check(dir.path())[0], IndexState::Valid);
}

/// (Plan 10) T7: a reader that landed through an index, and damage and a repair after it: it ends where a reader that had
/// scanned to the same place may end, and its next seek puts it back on the current chain.
#[test]
fn a_repair_after_the_landing() {
    // The record the damage empties: after the landing record, and before it.
    for (lost, landing_first) in [(2_300, true), (500, true), (500, false), (2_300, false)] {
        for repair in [Repair::InPlace, Repair::Replaced] {
            let (dir, writer) = indexed(64, 1);
            drop(writer);
            let stale = Stale {
                index: Vec::new(),
                at: lost,
                dir,
            };
            let marker = stale.path().join("checkpoint");
            std::fs::remove_file(&marker).unwrap();
            let mut reader = open(stale.path(), (false, false));
            // Readers stopped by step 2: no marker.
            for kind in &KINDS[1..] {
                let mut other = open(stale.path(), *kind);
                other.seek(2_000).unwrap();
                assert!(!other.seek_used_index());
            }
            let case = format!("{lost}, {landing_first}, {repair:?}");
            if landing_first {
                reader.seek(2_000).unwrap();
                assert!(reader.seek_used_index());
            }
            stale.damage(repair);
            if !landing_first {
                reader.seek(2_000).unwrap();
                // Damage that left the seal and the landing record: the jump does not see it.
                assert_eq!(
                    reader.seek_used_index(),
                    repair == Repair::Replaced,
                    "{case}"
                );
            }
            let mut writer = Writer::open(stale.path(), config()).unwrap();
            assert_eq!(writer.next_seq(), lost);
            fill(&mut writer, stale.path(), 2, 100, 1);
            let (got, end) = read_up_to(&mut reader, usize::MAX);
            let end = end.unwrap();
            assert!(
                end == "end" || end.contains("Discarded") || end.contains("Corrupt"),
                "{case}: {end}"
            );
            if repair == Repair::Replaced {
                assert!(end.contains("Discarded"), "{case}: {end}");
            }
            for (i, record) in got.iter().enumerate() {
                assert_eq!(record.seq, 2_000 + i as u64, "{case}");
                assert!(record.payload[0] == 1 || record.seq >= lost, "{case}");
            }
            for target in [reader.position(), 2_000] {
                agrees(stale.path(), &mut reader, (false, false), target, 10);
            }
        }
    }
}

// ---- Any history (the property test of tests/seek.rs, with indexes) ----

#[derive(Debug, Clone)]
enum Op {
    Append {
        n: u64,
        len: usize,
    },
    Checkpoint,
    Read(usize),
    Seek {
        permille: u64,
        read: usize,
    },
    Ahead {
        by: u64,
        read: usize,
    },
    /// Builds the indexes (the writer has the journal open).
    Build {
        rebuild: bool,
    },
    /// Flips a bit of an index, with a checksum that fits or not.
    Flip {
        index: usize,
        bit: usize,
        fit: bool,
    },
    /// Removes an index.
    Remove {
        index: usize,
    },
    /// Records from this thousandth of the uncovered newest segment on are lost, and the writer repairs the journal.
    Repair {
        lost: u64,
        len: usize,
        in_place: bool,
        aware: bool,
    },
    /// The marker is lost, and so are the records from this thousandth of the journal on.
    LoseMarker {
        permille: u64,
        len: usize,
        in_place: bool,
        aware: bool,
    },
    Restart {
        aware: bool,
    },
    Retain(usize),
    ToEnd,
}

fn op() -> impl Strategy<Value = Op> {
    let aware = || prop_oneof![1 => Just(true), 1 => Just(false)];
    prop_oneof![
        8 => (50_u64..300, 0_usize..2500).prop_map(|(n, len)| Op::Append { n, len }),
        6 => Just(Op::Checkpoint),
        3 => (1_usize..60).prop_map(Op::Read),
        12 => (0_u64..1050, 0_usize..20).prop_map(|(permille, read)| Op::Seek { permille, read }),
        6 => (0_u64..400, 0_usize..20).prop_map(|(by, read)| Op::Ahead { by, read }),
        10 => any::<bool>().prop_map(|rebuild| Op::Build { rebuild }),
        1 => (0_usize..8, 0_usize..4_000, any::<bool>())
            .prop_map(|(index, bit, fit)| Op::Flip { index, bit, fit }),
        1 => (0_usize..8).prop_map(|index| Op::Remove { index }),
        1 => (1_u64..20, 0_usize..300, any::<bool>(), aware())
            .prop_map(|(lost, len, in_place, aware)| Op::Repair { lost, len, in_place, aware }),
        1 => (0_u64..1000, 0_usize..700, any::<bool>(), aware())
            .prop_map(|(permille, len, in_place, aware)| Op::LoseMarker { permille, len, in_place, aware }),
        1 => aware().prop_map(|aware| Op::Restart { aware }),
        1 => (2_usize..5).prop_map(Op::Retain),
        1 => Just(Op::ToEnd),
    ]
}

/// The model: a fresh reader without the option, whose seek always scans the segment from its start. Any sequence of
/// appends, rolls, checkpoints, index builds, damaged and removed indexes, repairs (in place and by replacement, from
/// an intact and from a lost marker, by a writer that removes indexes and by one that does not), removals and writer
/// restarts, with readers of every kind reading and seeking in between.
#[test]
fn every_seek_agrees_with_one_without_an_index() {
    let used = AtomicU64::new(0);
    let mut runner = TestRunner::new(Config {
        cases: 96,
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(&proptest::collection::vec(op(), 1..60), |ops| {
            run_ops(&ops, &used);
            Ok(())
        })
        .unwrap();
    let used = used.into_inner();
    println!("{used} seeks used an index");
    assert!(used > 300, "only {used} seeks used an index");
}

/// The long version: more cases.
#[test]
#[ignore = "long"]
fn every_seek_agrees_with_one_without_an_index_long() {
    let used = AtomicU64::new(0);
    let mut runner = TestRunner::new(Config {
        cases: 400,
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(&proptest::collection::vec(op(), 1..90), |ops| {
            run_ops(&ops, &used);
            Ok(())
        })
        .unwrap();
    println!("{} seeks used an index", used.into_inner());
}

/// Opens the writer; unless `aware`, as one that knows of no indexes.
fn reopen(dir: &Path, config: &WriterConfig, aware: bool) -> Writer {
    fault::clear();
    if !aware {
        for _ in 0..2 {
            fault::fail_next(Point::SeekIndexList, libc::EIO);
        }
    }
    let writer = Writer::open(dir, config.clone()).unwrap();
    fault::clear();
    writer
}

fn run_ops(ops: &[Op], used: &AtomicU64) {
    let dir = tempfile::tempdir().unwrap();
    // Two blocks, so one entry.
    let config = config().segment_size(2 * SMALL_SEGMENT);
    let mut writer = reopen(dir.path(), &config, true);
    let mut tag = 1_u8;
    for _ in 0..400 {
        append(&mut writer, tag, 600);
    }
    writer.checkpoint().unwrap();
    let mut readers = KINDS.map(|kind| open(dir.path(), kind));
    let nth_index = |n: usize| {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(index_dir(dir.path()))
            .into_iter()
            .flatten()
            .map(|e| e.unwrap().path())
            .collect();
        paths.sort();
        (!paths.is_empty()).then(|| paths.swap_remove(n % paths.len()))
    };
    for op in ops {
        match *op {
            Op::Append { n, len } => {
                for i in 0..n {
                    append(&mut writer, tag, (len * (i as usize % 3)) / 2);
                }
            }
            Op::Checkpoint => writer.checkpoint().unwrap(),
            Op::Read(n) => {
                for reader in &mut readers {
                    let _ = read_up_to(reader, n);
                }
            }
            Op::Seek { permille, read } => {
                let first = segments(dir.path())[0].0;
                let target = first + (writer.next_seq() - first) * permille / 1000;
                for (reader, kind) in readers.iter_mut().zip(KINDS) {
                    let by = agrees(dir.path(), reader, kind, target, read);
                    used.fetch_add(u64::from(by), Ordering::Relaxed);
                }
            }
            Op::Ahead { by, read } => {
                for (reader, kind) in readers.iter_mut().zip(KINDS) {
                    let target = reader.position() + by;
                    let by = agrees(dir.path(), reader, kind, target, read);
                    used.fetch_add(u64::from(by), Ordering::Relaxed);
                }
            }
            Op::Build { rebuild } => match seek_index::build(dir.path(), rebuild) {
                Ok(_) => {}
                // Never checkpointed.
                Err(Error::Locked { .. }) => {
                    assert!(
                        tickertape::inspect::read_checkpoint(dir.path())
                            .unwrap()
                            .is_none()
                    );
                    drop(writer);
                    seek_index::build(dir.path(), rebuild).unwrap();
                    writer = reopen(dir.path(), &config, false);
                }
                Err(e) => panic!("{e}"),
            },
            Op::Flip { index, bit, fit } => {
                if let Some(path) = nth_index(index) {
                    let mut bytes = std::fs::read(&path).unwrap();
                    let bit = bit % (bytes.len() * 8);
                    bytes[bit / 8] ^= 1 << (bit % 8);
                    if fit {
                        codec::write_crc(&mut bytes);
                    }
                    std::fs::write(&path, bytes).unwrap();
                }
            }
            Op::Remove { index } => {
                if let Some(path) = nth_index(index) {
                    std::fs::remove_file(path).unwrap();
                }
            }
            Op::Restart { aware } => {
                drop(writer);
                writer = reopen(dir.path(), &config, aware);
            }
            Op::Repair {
                lost,
                len,
                in_place,
                aware,
            } => {
                let newest = segments(dir.path()).last().unwrap().0;
                let covered = tickertape::inspect::read_checkpoint(dir.path())
                    .unwrap()
                    .map_or(0, |c| c.next_seq);
                let next = writer.next_seq();
                let at = next.saturating_sub(lost).max(covered).max(newest);
                if at >= next {
                    continue;
                }
                drop(writer);
                lose_from(dir.path(), at, in_place);
                writer = reopen(dir.path(), &config, aware);
                assert_eq!(writer.next_seq(), at);
                tag = tag % 250 + 1;
                for _ in 0..lost + 3 {
                    append(&mut writer, tag, len);
                }
            }
            Op::LoseMarker {
                permille,
                len,
                in_place,
                aware,
            } => {
                let first = segments(dir.path())[0].0;
                let next = writer.next_seq();
                let at = first + (next - first) * permille / 1000;
                drop(writer);
                std::fs::remove_file(dir.path().join("checkpoint")).unwrap();
                lose_from(dir.path(), at, in_place);
                writer = reopen(dir.path(), &config, aware);
                assert_eq!(writer.next_seq(), at);
                tag = tag % 250 + 1;
                for _ in 0..(next - at).min(400) + 3 {
                    append(&mut writer, tag, len);
                }
                // A durable-only reader that applied the lost marker is C13's exception.
                for (reader, kind) in readers.iter_mut().zip(KINDS) {
                    if kind.1 {
                        *reader = open(dir.path(), kind);
                    }
                }
            }
            Op::Retain(keep) => {
                let plan =
                    retention::plan(dir.path(), &Policy::default().max_segments(keep)).unwrap();
                retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
            }
            Op::ToEnd => {
                for (reader, kind) in readers.iter_mut().zip(KINDS) {
                    reader.seek_to_end().unwrap();
                    let mut fresh = open(dir.path(), kind);
                    let (_, end) = read_up_to(&mut fresh, usize::MAX);
                    assert_eq!(end.as_deref(), Some("end"));
                    assert_eq!(reader.position(), fresh.position(), "{kind:?}");
                }
            }
        }
    }
    // Whatever happened, no index a reader would use is off the chain.
    for segment in seek_index::check(dir.path()).unwrap().segments {
        assert_ne!(segment.state, IndexState::Wrong, "{segment:?}");
    }
}

/// Makes the journal's records from `at` on be lost to the next recovery: in place (the later segments are gone, and the
/// commit words and the seal after the record before `at` empty), or by damage in record `at`.
fn lose_from(dir: &Path, at: u64, in_place: bool) {
    let segs = segments(dir);
    let (i, (_, path)) = segs
        .iter()
        .enumerate()
        .rev()
        .find(|(_, s)| s.0 <= at)
        .unwrap();
    let (records, seal) = chain(path);
    if !in_place {
        if let Some(record) = records.iter().find(|r| r.seq == at) {
            // A field every record has.
            open_raw(path)
                .write_all_at(&[0xEE], record.offset + 33)
                .unwrap();
        }
        return;
    }
    for (_, later) in &segs[i + 1..] {
        std::fs::remove_file(later).unwrap();
    }
    let raw = open_raw(path);
    if raw.metadata().unwrap().len() >= seal + 8 {
        raw.write_all_at(&[0; 8], seal).unwrap();
    }
    for record in records.iter().filter(|r| r.seq >= at) {
        raw.write_all_at(&[0; 4], record.offset).unwrap();
    }
}

// ---- The builder, its fence, and writers ----

/// One step of an interleaving: which party runs on until its next point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Turn {
    Builder,
    Writer,
}

/// A party on a thread of its own that stops at its points until it is told to go on.
struct Party {
    go: mpsc::Sender<()>,
    stopped: mpsc::Receiver<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Party {
    /// Starts `run` on a thread, stopped before its first step. It stops at each of `points` (the same point twice is its
    /// first two passes) and where `run` calls what it is given.
    fn start(points: &'static [Point], run: impl FnOnce(&dyn Fn()) + Send + 'static) -> Party {
        let (go, wait) = mpsc::channel();
        let (tell, stopped) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let wait = Rc::new(wait);
            let stop = {
                let (wait, tell) = (Rc::clone(&wait), tell.clone());
                move || {
                    tell.send(()).unwrap();
                    wait.recv().unwrap();
                }
            };
            fault::clear();
            // Armed actions of one point run one per pass, in order.
            for point in points {
                fault::run_at(*point, 0, stop.clone());
            }
            stop();
            run(&stop);
        });
        stopped.recv().unwrap();
        Party {
            go,
            stopped,
            thread: Some(thread),
        }
    }

    /// Lets the party run to its next point; false if it has finished.
    fn step(&mut self) -> bool {
        if self.thread.is_none() {
            return false;
        }
        self.go.send(()).unwrap();
        if self.stopped.recv().is_err() {
            self.finish();
            return false;
        }
        true
    }

    fn finish(&mut self) {
        while self.thread.is_some() && self.go.send(()).is_ok() && self.stopped.recv().is_ok() {}
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

/// How the marker is when the writer opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerAtOpen {
    /// Past the segment, as when the build began.
    Intact,
    /// Lost right before the open, with damage in the segment; it was past the segment before.
    Lost,
    /// There never was one, and the segment is damaged.
    Never,
}

/// (Plan 12) A build and a writer's open with its recovery, its new history, a roll and a checkpoint past the segment, in one
/// interleaving of their steps. No index of the old history survives, and none that a reader would use is wrong.
fn interleave(marker: MarkerAtOpen, repair: Repair, len: usize, schedule: &[Turn]) {
    let dir = tempfile::tempdir().unwrap();
    let config = config().segment_size(2 * SMALL_SEGMENT);
    let mut writer = Writer::open(dir.path(), config.clone()).unwrap();
    fill(&mut writer, dir.path(), 1, 64, 1);
    if marker != MarkerAtOpen::Never {
        writer.checkpoint().unwrap();
    }
    drop(writer);
    let path = dir.path().to_path_buf();
    let mut builder = Party::start(
        &[
            Point::SeekIndexFence,
            Point::SeekIndexEligible,
            Point::SeekIndexBeforeRename,
            Point::SeekIndexAfterRename,
        ],
        {
            let path = path.clone();
            move |_| match seek_index::build(&path, false) {
                // Locked: no marker, and the writer is open. A dangling marker cannot be.
                Ok(_) | Err(Error::Locked { .. }) => {}
                Err(e) => panic!("{e}"),
            }
        },
    );
    let mut opener = Party::start(
        &[
            Point::SeekIndexListed,
            Point::SeekIndexListed,
            Point::SeekIndexRemoved,
        ],
        {
            let path = path.clone();
            move |stop| {
                if marker != MarkerAtOpen::Intact {
                    if marker == MarkerAtOpen::Lost {
                        std::fs::remove_file(path.join("checkpoint")).unwrap();
                    }
                    lose_from(&path, 700, repair == Repair::InPlace);
                }
                // A builder without a marker holds the lock for an instant.
                let mut writer = tickertape_tests::reopen_writer(&path, config);
                if marker != MarkerAtOpen::Intact {
                    assert_eq!(writer.next_seq(), 700);
                }
                stop();
                fill(&mut writer, &path, 2, len, 1);
                writer.checkpoint().unwrap();
            }
        },
    );
    let case = format!("{marker:?}, {repair:?}, {len}, {schedule:?}");
    for turn in schedule {
        match turn {
            Turn::Builder => builder.step(),
            Turn::Writer => opener.step(),
        };
    }
    builder.finish();
    opener.finish();
    let before = std::fs::read(index_path(dir.path(), 0)).ok();
    // Whatever index is left is the one a build makes now.
    let state = check(dir.path())[0];
    match before {
        Some(_) => assert_eq!(state, IndexState::Valid, "{case}"),
        None => assert_eq!(state, IndexState::Missing, "{case}"),
    }
    seek_index::build(dir.path(), true).unwrap();
    let rebuilt = std::fs::read(index_path(dir.path(), 0)).unwrap();
    if let Some(before) = before {
        assert_eq!(before, rebuilt, "{case}");
    }
    // Nothing unfinished is left, unless the writer opened after the build was done.
    assert!(
        index_names(dir.path()).iter().all(|n| n.ends_with(".idx")),
        "{case}"
    );
}

/// Every schedule of `a` builder steps and `b` writer steps.
fn schedules(a: usize, b: usize) -> Vec<Vec<Turn>> {
    if a == 0 && b == 0 {
        return vec![Vec::new()];
    }
    let mut out = Vec::new();
    for (turn, a, b) in [
        (Turn::Builder, a.wrapping_sub(1), b),
        (Turn::Writer, a, b.wrapping_sub(1)),
    ] {
        if a != usize::MAX && b != usize::MAX {
            for mut rest in schedules(a, b) {
                rest.insert(0, turn);
                out.push(rest);
            }
        }
    }
    out
}

/// The cases of [`interleave`], `every`th of them, starting at a place of its own for each marker and repair.
fn interleavings(every: usize) {
    // Builder: to each of its four points, and to its end. Writer: to each of three points, to the end of its open, and
    // to its end.
    let schedules = schedules(5, 5);
    assert_eq!(schedules.len(), 252);
    let mut cases = Vec::new();
    for marker in [
        MarkerAtOpen::Intact,
        MarkerAtOpen::Lost,
        MarkerAtOpen::Never,
    ] {
        for repair in [Repair::InPlace, Repair::Replaced] {
            // Records of the same size, so that the new history ends alike, and shorter ones.
            for len in [64, 40] {
                let skip =
                    marker == MarkerAtOpen::Intact && (repair == Repair::Replaced || len == 40);
                let first = cases.len() % every;
                for schedule in schedules.iter().skip(first).step_by(every) {
                    if !skip {
                        cases.push((marker, repair, len, schedule.clone()));
                    }
                }
            }
        }
    }
    let next = AtomicU64::new(0);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                while let Some((marker, repair, len, schedule)) =
                    cases.get(next.fetch_add(1, Ordering::Relaxed) as usize)
                {
                    interleave(*marker, *repair, *len, schedule);
                }
            });
        }
    });
}

#[test]
fn no_index_of_an_old_history_survives_any_interleaving() {
    interleavings(13);
}

/// Every interleaving at the fault points. (The exhaustive model of the protocol is in `seek_index/model.rs`.)
#[test]
#[ignore = "long"]
fn no_index_of_an_old_history_survives_any_interleaving_long() {
    interleavings(1);
}

/// (Plan 12) A writer that opens during each attempt makes the build start again, at most three times.
#[test]
fn a_build_starts_again_at_most_three_times() {
    for opens in [1, 3, 4] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), config()).unwrap();
        fill(&mut writer, dir.path(), 1, 64, 1);
        writer.checkpoint().unwrap();
        drop(writer);
        fault::clear();
        let fences = Rc::new(Cell::new(0));
        for pass in 0..6 {
            let seen = Rc::clone(&fences);
            fault::run_at(Point::SeekIndexFence, 0, move || seen.set(seen.get() + 1));
            if pass < opens {
                let path = dir.path().to_path_buf();
                fault::run_at(Point::SeekIndexBeforeRename, 0, move || {
                    drop(Writer::open(path, config()).unwrap());
                });
            }
        }
        let result = seek_index::build(dir.path(), false);
        fault::clear();
        assert_eq!(fences.get(), 4.min(opens + 1));
        if opens == 4 {
            assert!(matches!(result, Err(Error::Io { .. })), "{result:?}");
            assert!(index_names(dir.path()).is_empty());
        } else {
            assert_eq!(result.unwrap().segments[0].state, IndexState::Built);
            assert_eq!(check(dir.path())[0], IndexState::Valid);
            assert_eq!(index_names(dir.path()).len(), 1);
        }
    }
}

/// (Plan 12) The segment changes under a build: removed, or replaced, between the walk and the rename, or after it. What the
/// builder put in place it takes away, but not what another builder did.
#[test]
fn a_segment_that_goes_during_a_build_keeps_no_index() {
    for (point, other) in [
        (Point::SeekIndexEligible, false),
        (Point::SeekIndexBeforeRename, false),
        (Point::SeekIndexAfterRename, false),
        (Point::SeekIndexAfterRename, true),
    ] {
        for replaced in [false, true] {
            let (dir, writer) = {
                let dir = tempfile::tempdir().unwrap();
                let mut writer = Writer::open(dir.path(), config()).unwrap();
                fill(&mut writer, dir.path(), 1, 64, 2);
                writer.checkpoint().unwrap();
                (dir, writer)
            };
            let path = segments(dir.path())[0].1.clone();
            fault::clear();
            let (p, d) = (path.clone(), dir.path().to_path_buf());
            fault::run_at(point, 0, move || {
                if replaced {
                    let copy = d.join("copy");
                    std::fs::copy(&p, &copy).unwrap();
                    std::fs::rename(&copy, &p).unwrap();
                } else {
                    retention::apply(
                        &d,
                        &retention::plan(&d, &Policy::default().max_segments(2)).unwrap(),
                        &Action::Delete,
                    )
                    .unwrap();
                    assert!(!p.exists());
                }
                if other {
                    let index = index_path(&d, 0);
                    let bytes = [std::fs::read(&index).unwrap_or_default(), vec![1]].concat();
                    std::fs::write(d.join("other"), bytes).unwrap();
                    std::fs::rename(d.join("other"), &index).unwrap();
                }
            });
            let report = seek_index::build(dir.path(), false).unwrap();
            let case = format!("{point:?}, other {other}, replaced {replaced}");
            let state = report.segments[0].state;
            if replaced && point == Point::SeekIndexEligible {
                // Opened after that, so it is the file that is there.
                assert_eq!(state, IndexState::Built, "{case}");
            } else {
                assert_eq!(state, IndexState::Vanished, "{case}");
                assert_eq!(index_path(dir.path(), 0).exists(), other, "{case}");
            }
            assert_eq!(report.segments[1].state, IndexState::Built, "{case}");
            drop(writer);
        }
    }
}

/// (Plan 12) The marker is made durable before the walk, once, and a run that cannot do so builds nothing.
#[test]
fn the_marker_is_made_durable_before_the_walk() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    fill(&mut writer, dir.path(), 1, 64, 3);
    writer.checkpoint().unwrap();
    fault::clear();
    fault::fail_next(Point::RetentionMarkerSync, libc::EIO);
    let walked = Rc::new(Cell::new(false));
    let seen = Rc::clone(&walked);
    fault::run_at(Point::SeekIndexEligible, 0, move || seen.set(true));
    assert!(matches!(
        seek_index::build(dir.path(), false),
        Err(Error::Io { .. })
    ));
    assert!(!walked.get());
    assert!(
        index_names(dir.path()).is_empty(),
        "the temporary file is removed"
    );
    fault::clear();
    // One sync for the one slot: a second would fail.
    let path = dir.path().to_path_buf();
    fault::run_at(Point::SeekIndexEligible, 0, move || {
        assert_eq!(index_names(&path).len(), 1, "the fence");
        fault::fail_next(Point::RetentionMarkerSync, libc::EIO);
    });
    assert_eq!(build(dir.path())[..3], [IndexState::Built; 3]);
    fault::clear();
    // Covered by the marker only up to its segment.
    assert_eq!(
        build(dir.path()),
        [
            IndexState::Valid,
            IndexState::Valid,
            IndexState::Valid,
            IndexState::NotCovered
        ]
    );
    assert_eq!(
        check(dir.path()),
        [
            IndexState::Valid,
            IndexState::Valid,
            IndexState::Valid,
            IndexState::NotCovered
        ]
    );
}

/// (Plan 12) What decides is the marker read behind the fence, not the one read before it: here the marker goes back to
/// an older one, which names the segment itself, in between.
#[test]
fn the_marker_behind_the_fence_decides() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    fill(&mut writer, dir.path(), 1, 64, 1);
    writer.checkpoint().unwrap();
    let marker = dir.path().join("checkpoint");
    let older = std::fs::read(&marker).unwrap();
    fill(&mut writer, dir.path(), 1, 64, 2);
    writer.checkpoint().unwrap();
    drop(writer);
    fault::clear();
    // The second segment's fence.
    fault::run_at(Point::SeekIndexFence, 1, move || {
        std::fs::write(&marker, &older).unwrap();
    });
    let built = build(dir.path());
    assert_eq!(built[..2], [IndexState::Built, IndexState::NotCovered]);
    assert_eq!(index_names(dir.path()).len(), 1);
}

/// (Plan 12) A write error, a name that is taken, a file of a newer version, and what `rebuild` does with it.
#[test]
fn errors_and_other_files_in_the_way_of_a_build() {
    let (dir, _writer) = indexed(64, 1);
    let index = index_path(dir.path(), 0);
    let good = std::fs::read(&index).unwrap();
    fault::clear();
    fault::fail_next(Point::SeekIndexWrite, libc::ENOSPC);
    let error = seek_index::build(dir.path(), true).unwrap_err();
    assert!(error.is_storage_full(), "{error}");
    assert_eq!(index_names(dir.path()).len(), 1);
    assert_eq!(std::fs::read(&index).unwrap(), good, "the old one stays");
    // A newer version is kept, whatever else is in it; `rebuild` replaces it.
    let mut newer = good.clone();
    newer[8] = 2;
    std::fs::write(&index, &newer[..50]).unwrap();
    let report = seek_index::build(dir.path(), false).unwrap();
    assert_eq!(report.segments[0].state, IndexState::Ignored);
    assert!(
        report.segments[0]
            .reason
            .as_ref()
            .unwrap()
            .contains("version 2")
    );
    assert_eq!(std::fs::read(&index).unwrap(), newer[..50]);
    assert_eq!(check(dir.path())[0], IndexState::Ignored);
    assert_eq!(
        seek_index::build(dir.path(), true).unwrap().segments[0].state,
        IndexState::Built
    );
    assert_eq!(std::fs::read(&index).unwrap(), good);
    // Version 0, or another file: replaced.
    for bytes in [
        [&good[..8], &[0, 0], &good[10..]].concat(),
        b"notes".to_vec(),
    ] {
        std::fs::write(&index, bytes).unwrap();
        assert_eq!(check(dir.path())[0], IndexState::Ignored);
        assert_eq!(build(dir.path())[0], IndexState::Built);
        assert_eq!(std::fs::read(&index).unwrap(), good);
    }
    // A directory there cannot be replaced.
    std::fs::remove_file(&index).unwrap();
    std::fs::create_dir(&index).unwrap();
    assert!(matches!(
        seek_index::build(dir.path(), false),
        Err(Error::Io { .. })
    ));
    assert_eq!(index_names(dir.path()).len(), 1);
    std::fs::remove_dir(&index).unwrap();
    // Names a builder would take: each is skipped.
    let path = dir.path().to_path_buf();
    let taken = Rc::new(Cell::new(0));
    let count = Rc::clone(&taken);
    fault::run_at(Point::SeekIndexFence, 0, move || {
        let name = index_names(&path).pop().unwrap();
        let parts: Vec<&str> = name.split('.').collect();
        assert_eq!(parts[2], std::process::id().to_string());
        let n: u64 = parts[3].parse().unwrap();
        assert!(n.abs_diff(tickertape::clock::realtime_ns()) < 60_000_000_000);
        count.set(n);
    });
    assert_eq!(build(dir.path())[0], IndexState::Built);
    assert!(taken.get() > 0);
    assert!(matches!(
        seek_index::build(dir.path().join("none"), false),
        Err(Error::Io { .. })
    ));
    let empty = tempfile::tempdir().unwrap();
    assert!(matches!(
        seek_index::build(empty.path(), false),
        Err(Error::NotFound { .. })
    ));
    assert!(matches!(
        seek_index::check(empty.path()),
        Err(Error::NotFound { .. })
    ));
    assert_eq!(seek_index::remove(empty.path()).unwrap(), 0);
    assert_eq!(std::fs::read_dir(empty.path()).unwrap().count(), 0);
}

/// The child of [`a_build_killed_at_any_point_leaves_nothing_wrong`] and of the writer's counterpart.
#[test]
fn child_killed_at_a_point() {
    let Ok(spec) = std::env::var("TT_SEEK_INDEX_CHILD") else {
        return;
    };
    let (what, dir) = spec.split_once(':').unwrap();
    let abort = || std::process::abort();
    fault::clear();
    match what {
        "fence" => fault::run_at(Point::SeekIndexFence, 0, abort),
        "eligible" => fault::run_at(Point::SeekIndexEligible, 0, abort),
        "before-rename" => fault::run_at(Point::SeekIndexBeforeRename, 0, abort),
        "after-rename" => fault::run_at(Point::SeekIndexAfterRename, 0, abort),
        "first-pass" => fault::run_at(Point::SeekIndexListed, 0, abort),
        "removed" => fault::run_at(Point::SeekIndexRemoved, 0, abort),
        _ => panic!("{what}"),
    }
    if matches!(what, "first-pass" | "removed") {
        let _ = Writer::open(dir, config());
    } else {
        let _ = seek_index::build(dir, true);
    }
    unreachable!("not killed");
}

fn kill_child(what: &str, dir: &Path) {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_killed_at_a_point", "--nocapture"])
        .env("TT_SEEK_INDEX_CHILD", format!("{what}:{}", dir.display()))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(libc::SIGABRT),
        "{what}"
    );
}

#[test]
fn a_build_killed_at_any_point_leaves_nothing_wrong() {
    for what in ["fence", "eligible", "before-rename", "after-rename"] {
        for existing in [false, true] {
            let (dir, writer) = indexed(64, 1);
            if !existing {
                seek_index::remove(dir.path()).unwrap();
            }
            kill_child(what, dir.path());
            let there = existing || what == "after-rename";
            assert_eq!(index_path(dir.path(), 0).exists(), there, "{what}");
            let left = usize::from(what != "after-rename");
            assert_eq!(
                index_names(dir.path()).len(),
                left + usize::from(there),
                "{what}"
            );
            let used = all_agree(dir.path(), &[2_000], 3);
            assert_eq!(used, if there { 4 } else { 0 });
            // The next run builds it, and the next open of a writer removes what was left.
            build(dir.path());
            assert_eq!(check(dir.path())[0], IndexState::Valid);
            drop(writer);
            let writer = Writer::open(dir.path(), config()).unwrap();
            assert_eq!(writer.recovery_report().seek_indexes_removed, left as u64);
            assert_eq!(index_names(dir.path()).len(), 1);
        }
    }
}

/// (Plan 7) A writer killed during or right after the removal: the next open finishes it, and recovers.
#[test]
fn a_writer_killed_around_the_removal() {
    for (what, repair) in ["first-pass", "removed"]
        .into_iter()
        .flat_map(|w| [(w, Repair::InPlace), (w, Repair::Replaced)])
    {
        let stale = Stale::new(repair, 1_300);
        let before = std::fs::read(&segments(stale.path())[0].1).unwrap();
        kill_child(what, stale.path());
        assert!(index_names(stale.path()).is_empty());
        assert_eq!(std::fs::read(&segments(stale.path())[0].1).unwrap(), before);
        let mut writer = tickertape_tests::reopen_writer(stale.path(), config());
        assert_eq!(writer.next_seq(), 1_300);
        fill(&mut writer, stale.path(), 2, 64, 1);
        all_agree(stale.path(), &stale.targets(), 3);
    }
}

/// (Plan 13) Without a marker: every sealed segment, and only if no writer is open.
#[test]
fn a_directory_without_a_marker() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    fill(&mut writer, dir.path(), 1, 64, 2);
    let lock = dir.path().join("writer.lock");
    let held = std::fs::read(&lock).unwrap();
    let modified = std::fs::metadata(&lock).unwrap().modified().unwrap();
    match seek_index::build(dir.path(), false) {
        Err(Error::Locked { pid, .. }) => assert_eq!(pid, Some(std::process::id())),
        other => panic!("{other:?}"),
    }
    assert_eq!(std::fs::read(&lock).unwrap(), held);
    assert_eq!(
        std::fs::metadata(&lock).unwrap().modified().unwrap(),
        modified
    );
    assert!(index_names(dir.path()).is_empty());
    drop(writer);
    let built = [IndexState::Built, IndexState::Built, IndexState::NotSealed];
    // While another builder probes: shared locks do not refuse each other. And a child forked then holds a copy of the
    // descriptor, not the lock.
    let path = dir.path().to_path_buf();
    let child = Rc::new(Cell::new(0));
    let forked = Rc::clone(&child);
    fault::clear();
    fault::run_at(Point::SeekIndexProbe, 0, move || {
        fault::run_at(Point::SeekIndexProbe, 0, move || {
            // SAFETY: the child only sleeps and exits.
            let pid = unsafe { libc::fork() };
            if pid == 0 {
                // SAFETY: async-signal-safe calls.
                unsafe {
                    libc::sleep(20);
                    libc::_exit(0);
                }
            }
            forked.set(pid);
        });
        assert_eq!(
            states(&seek_index::build(&path, true).unwrap().segments)[1],
            IndexState::Built
        );
    });
    // The inner build's index of the second segment is kept.
    let kept = [IndexState::Built, IndexState::Valid, IndexState::NotSealed];
    assert_eq!(build(dir.path()), kept);
    assert!(child.get() > 0);
    let writer = Writer::open(dir.path(), config()).unwrap();
    assert_eq!(
        writer.recovery_report().seek_indexes_removed,
        2,
        "no marker: all of them"
    );
    drop(writer);
    // SAFETY: our child.
    unsafe {
        libc::kill(child.get(), libc::SIGKILL);
        libc::waitpid(child.get(), std::ptr::null_mut(), 0);
    }
    // A writer restart during a run succeeds, and the run starts over.
    let path = dir.path().to_path_buf();
    fault::run_at(Point::SeekIndexBeforeRename, 0, move || {
        drop(Writer::open(path, config()).unwrap());
    });
    assert_eq!(build(dir.path()), built);
    seek_index::remove(dir.path()).unwrap();

    // No lock file: an archive. A FIFO, a symlink, a directory or an unreadable file there says nothing.
    std::fs::remove_file(&lock).unwrap();
    assert_eq!(build(dir.path()), built);
    assert!(!lock.exists());
    seek_index::remove(dir.path()).unwrap();
    for case in 0..4 {
        match case {
            0 => {
                let fifo = std::ffi::CString::new(lock.to_str().unwrap()).unwrap();
                // SAFETY: a valid C string.
                assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
            }
            1 => std::os::unix::fs::symlink("elsewhere", &lock).unwrap(),
            2 => std::fs::create_dir(&lock).unwrap(),
            _ => {
                std::fs::write(&lock, b"").unwrap();
                std::fs::set_permissions(&lock, Permissions::from_mode(0o000)).unwrap();
            }
        }
        if case < 3 || std::fs::read(&lock).is_err() {
            let result = seek_index::build(dir.path(), false);
            assert!(
                matches!(result, Err(Error::Io { .. })),
                "{case}: {result:?}"
            );
            assert!(index_names(dir.path()).is_empty());
        }
        if case == 2 {
            std::fs::remove_dir(&lock).unwrap();
        } else {
            std::fs::remove_file(&lock).unwrap();
        }
    }
}

/// (Plan 14) Owner, group and mode of the directory and the files, whatever the umask.
#[test]
fn permissions_of_what_a_build_creates() {
    // The umask belongs to the process: in a child of its own.
    for umask in ["077", "002"] {
        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(r#"umask "$1"; exec "$0" --exact child_permissions --nocapture"#)
            .arg(std::env::current_exe().unwrap())
            .arg(umask)
            .env("TT_SEEK_INDEX_PERMISSIONS", umask)
            .status()
            .unwrap();
        assert!(status.success(), "umask {umask}");
    }
}

#[test]
fn child_permissions() {
    if std::env::var("TT_SEEK_INDEX_PERMISSIONS").is_err() {
        return;
    }
    let mode = |path: &Path| std::fs::metadata(path).unwrap().mode() & 0o7777;
    let chmod = |path: &Path, m| std::fs::set_permissions(path, Permissions::from_mode(m)).unwrap();
    let gid = |path: &Path| std::fs::metadata(path).unwrap().gid();
    // Another group of this user, if there is one.
    let groups: Vec<u32> = String::from_utf8(
        std::process::Command::new("id")
            .arg("-G")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .split_whitespace()
    .map(|g| g.parse().unwrap())
    .collect();
    for (dir_mode, file_mode, expected) in [
        (0o750, 0o640, 0o640),
        (0o700, 0o600, 0o600),
        (0o755, 0o444, 0o644),
        (0o2770, 0o660, 0o640),
        (0o1775, 0o664, 0o644),
    ] {
        for group in groups.iter().take(2) {
            let dir = tempfile::tempdir().unwrap();
            let mut writer = Writer::open(dir.path(), config()).unwrap();
            fill(&mut writer, dir.path(), 1, 64, 1);
            writer.checkpoint().unwrap();
            drop(writer);
            let segment = segments(dir.path())[0].1.clone();
            std::os::unix::fs::chown(&segment, None, Some(*group)).unwrap();
            std::os::unix::fs::chown(dir.path(), None, Some(*group)).unwrap();
            chmod(&segment, file_mode);
            chmod(dir.path(), dir_mode);
            if mode(dir.path()) != dir_mode {
                continue; // The platform does not let us set that bit with that group.
            }
            assert_eq!(build(dir.path())[0], IndexState::Built);
            let (made, index) = (index_dir(dir.path()), index_path(dir.path(), 0));
            assert_eq!(mode(&made), dir_mode & 0o777, "{dir_mode:o}");
            assert_eq!(gid(&made), *group);
            assert_eq!(mode(&index), expected);
            assert_eq!(gid(&index), *group);
            chmod(&segment, 0o644);
            assert_eq!(all_agree(dir.path(), &[2_000], 3), 4);
        }
    }

    // Made group-writable: readers ignore it, `check` says so, and a build makes it anew.
    let (dir, _writer) = indexed(64, 1);
    let index = index_path(dir.path(), 0);
    chmod(&index, 0o664);
    let report = seek_index::check(dir.path()).unwrap();
    assert_eq!(report.segments[0].state, IndexState::Valid);
    assert!(report.segments[0].ignored_by_readers);
    assert!(report.segments[0].reason.is_some());
    assert_eq!(all_agree(dir.path(), &[2_000], 3), 0);
    assert_eq!(build(dir.path())[0], IndexState::Built);
    assert!(!seek_index::check(dir.path()).unwrap().segments[0].ignored_by_readers);

    // A directory that is world-writable: refused, for a build, the journal's or its own.
    let refused = |dir: &Path| match seek_index::build(dir, true) {
        Err(Error::Io { source, .. }) => {
            assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
        }
        other => panic!("{other:?}"),
    };
    chmod(&index_dir(dir.path()), 0o777);
    refused(dir.path());
    chmod(&index_dir(dir.path()), 0o775);
    assert_eq!(
        build(dir.path())[0],
        IndexState::Valid,
        "group-writable is accepted"
    );
    seek_index::remove(dir.path()).unwrap();
    std::fs::remove_dir(index_dir(dir.path())).unwrap();
    chmod(dir.path(), 0o777);
    refused(dir.path());
    assert!(!index_dir(dir.path()).exists(), "nothing created");
    // Read-only.
    chmod(dir.path(), 0o555);
    if std::fs::write(dir.path().join("probe"), b"").is_err() {
        assert!(matches!(
            seek_index::build(dir.path(), false),
            Err(Error::Io { .. })
        ));
    }
    chmod(dir.path(), 0o755);
    assert_eq!(build(dir.path())[0], IndexState::Built);
}

/// (Plan 14, as root (`sudo -E cargo test ..) --ignored as_root`): another user's journal, and files of another owner.
#[test]
#[ignore = "does something only as root"]
fn as_root_another_users_journal_is_left_alone() {
    // SAFETY: no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skipped: not root");
        return;
    }
    let chown = |path: &Path, uid| std::os::unix::fs::chown(path, Some(uid), None).unwrap();
    let refused = |dir: &Path| {
        assert!(matches!(
            seek_index::build(dir, false),
            Err(Error::Io { ref source, .. }) if source.kind() == std::io::ErrorKind::PermissionDenied
        ));
    };
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), config()).unwrap();
    fill(&mut writer, dir.path(), 1, 64, 1);
    writer.checkpoint().unwrap();
    let segment = segments(dir.path())[0].1.clone();
    // Its segment: the directory is not created for it.
    chown(&segment, 1234);
    assert_eq!(build(dir.path())[0], IndexState::NotPermitted);
    assert!(!index_dir(dir.path()).exists());
    chown(&segment, 0);
    // Its directory.
    chown(dir.path(), 1234);
    refused(dir.path());
    assert!(!index_dir(dir.path()).exists());
    chown(dir.path(), 0);
    assert_eq!(build(dir.path())[0], IndexState::Built);
    assert_eq!(all_agree(dir.path(), &[2_000], 3), 4);
    // An index of root's for another's segment is used; one of another's for root's is not.
    chown(&segment, 1234);
    assert_eq!(all_agree(dir.path(), &[2_000], 3), 4);
    chown(&index_path(dir.path(), 0), 1234);
    assert_eq!(all_agree(dir.path(), &[2_000], 3), 4);
    chown(&segment, 0);
    assert_eq!(all_agree(dir.path(), &[2_000], 3), 0);
    let report = seek_index::check(dir.path()).unwrap();
    assert!(report.segments[0].ignored_by_readers);
    // Its `seek-index/`.
    chown(&index_dir(dir.path()), 1234);
    refused(dir.path());
}

/// (Plan 15) Retention removes a segment's index after the segment, and does not mind if it cannot; `MoveTo` does not carry
/// it, and the archive gets its own.
#[test]
fn retention_and_archives() {
    let (dir, mut writer) = indexed(64, 5);
    let segs = segments(dir.path());
    let retain = |keep, action: &Action| {
        let plan = retention::plan(dir.path(), &Policy::default().max_segments(keep)).unwrap();
        retention::apply(dir.path(), &plan, action)
            .unwrap()
            .removed_segments
    };
    assert_eq!(retain(5, &Action::Delete), 1);
    assert!(!index_path(dir.path(), 0).exists());
    assert_eq!(index_names(dir.path()).len(), 4);
    // An index that cannot be removed.
    fault::clear();
    fault::fail_next(Point::SeekIndexUnlink, libc::EIO);
    assert_eq!(retain(4, &Action::Delete), 1);
    fault::clear();
    assert!(!segs[1].1.exists());
    assert!(index_path(dir.path(), segs[1].0).exists());
    // A later kind's file is left, too.
    let later = index_dir(dir.path()).join(format!("{:020}.idx2", segs[2].0));
    std::fs::write(&later, b"x").unwrap();
    let archive = tempfile::tempdir().unwrap();
    assert_eq!(retain(3, &Action::MoveTo(archive.path().into())), 1);
    assert!(later.exists());
    assert!(!index_path(dir.path(), segs[2].0).exists());
    assert_eq!(std::fs::read_dir(archive.path()).unwrap().count(), 1);
    // The archive: no marker, no lock, every segment sealed.
    assert_eq!(build(archive.path()), [IndexState::Built]);
    for kind in KINDS {
        let target = segs[2].0 + 2_000;
        let mut reader = open(archive.path(), kind);
        let used = agrees(archive.path(), &mut reader, kind, target, 3);
        // Mapped and durable-only readers use none in an archive.
        assert_eq!(used, kind == (false, false));
    }
    assert!(!archive.path().join("writer.lock").exists());
    // The writer's own retention.
    append(&mut writer, 1, 10);
    drop(writer);
    let own = config().retention(Policy::default().max_segments(2), Action::Delete);
    let writer = Writer::open(dir.path(), own).unwrap();
    // The two leftovers are orphans; then the open's own retention removed a segment and its index.
    assert_eq!(writer.recovery_report().seek_indexes_removed, 2);
    assert_eq!(writer.retention_report().removed_segments, 1);
    assert_eq!(index_names(dir.path()), [format!("{:020}.idx", segs[4].0)]);
}
