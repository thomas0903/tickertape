//! Tests of the checkpoint marker as the bound on the zero-copy reader's loads (ADR 0048 → Tests; the numbers are those
//! there). They are here, not in `tests/`, because they look at which bytes are loaded through the mapping and at the
//! bound itself. `tests/mapped_bound.rs` has the cases that need only the public interface.

use super::{COPIED, Reader, SlotOutcome};
use crate::format::{self, CheckpointSlot, SEAL_MARKER};
use crate::mmap::MAPPED_READS;
use crate::mmap::test_hooks::record_loads;
use crate::{CorruptReason, Error, NewRecord, OwnedRecord, ReaderConfig, Writer, WriterConfig};
use proptest::prelude::*;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

const SEGMENT: u64 = 65_536;
const FIRST: u64 = format::SEGMENT_HEADER_LEN as u64;

fn writer(dir: &Path) -> Writer {
    Writer::open(dir, WriterConfig::default().segment_size(SEGMENT)).unwrap()
}

fn payload(seq: u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (seq as usize * 31 + i * 7 + 1) as u8)
        .collect()
}

fn append(writer: &mut Writer, len: usize) {
    let seq = writer.next_seq();
    writer
        .append(&NewRecord {
            schema_id: 1,
            venue_ts_ns: seq,
            local_ts_ns: 2,
            payload: &payload(seq, len),
        })
        .unwrap();
}

fn open_with(dir: &Path, mapped: bool, config: ReaderConfig) -> Reader {
    let config = config.notify(false);
    if mapped {
        // SAFETY: besides tickertape writers, only these tests modify the journal, where no reader holds a payload.
        unsafe { Reader::open_mapped(dir, config) }.unwrap()
    } else {
        Reader::open(dir, config).unwrap()
    }
}

fn open(dir: &Path, mapped: bool, durable: bool) -> Reader {
    open_with(dir, mapped, ReaderConfig::default().durable_only(durable))
}

fn raw(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap()
}

fn segment(dir: &Path, base: u64) -> PathBuf {
    crate::journal::segment_path(dir, base)
}

/// Replaces the marker by one that names `offset` in segment `base`.
fn put_marker(dir: &Path, base: u64, offset: u64, next_seq: u64) {
    let slot = format::encode_checkpoint_slot(&CheckpointSlot {
        generation: 1_000,
        next_seq,
        segment_base_seq: base,
        offset,
    });
    std::fs::write(dir.join(crate::journal::CHECKPOINT_FILE), slot).unwrap();
}

/// Whether the record found last is in the buffer.
fn is_copied(reader: &Reader) -> bool {
    reader.found.payload_at & COPIED != 0
}

/// The limit the reader's loads have now: [`NO_LIMIT`] if there is none but the segment's own size.
const NO_LIMIT: u64 = u64::MAX;

fn limit_now(reader: &Reader) -> u64 {
    if reader.bound < reader.seg.size {
        reader.bound - format::SEGMENT_TRAILER_LEN as u64
    } else {
        NO_LIMIT
    }
}

/// The frames of segment `base`: offset and length of each record.
fn frames(dir: &Path, base: u64) -> Vec<(u64, u32)> {
    let image = std::fs::read(segment(dir, base)).unwrap();
    let (mut at, mut seq, mut out) = (FIRST, base, Vec::new());
    while let Ok(format::Slot::Record {
        len, next_offset, ..
    }) = format::read_slot(&image[..], at, seq)
    {
        out.push((at, len));
        at = next_offset;
        seq += 1;
    }
    out
}

/// Every record from the cursor on, and how reading ended.
fn drain(reader: &mut Reader) -> (Vec<OwnedRecord>, String) {
    let mut records = Vec::new();
    loop {
        match reader.next_owned() {
            Ok(Some(record)) => records.push(record),
            other => return (records, format!("{other:?}")),
        }
    }
}

/// The outcome as the safe reader reports it.
fn plain(outcome: SlotOutcome) -> String {
    match outcome {
        SlotOutcome::Copied { len } => format!("{:?}", SlotOutcome::Record { len }),
        other => format!("{other:?}"),
    }
}

/// Test plan 3 and 4: every damaged length of a covered record, and some of a record that the bound ends in front of.
/// A crossing frame is not loaded through the mapping beyond its commit word, no load ends after the bound, and the
/// outcome is the safe reader's.
#[test]
fn no_load_past_the_limit() {
    if !MAPPED_READS {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut w = writer(dir.path());
    for i in 0..8 {
        append(&mut w, 20 + i * 13);
    }
    w.checkpoint().unwrap();
    for _ in 0..4 {
        append(&mut w, 100);
    }
    drop(w);
    let frames = frames(dir.path(), 0);
    let limit = frames[8].0;
    let file = raw(&segment(dir.path(), 0));
    let (mut crossed, mut inside) = (0, 0);
    for target in [0, 3, 7] {
        let at = frames[target].0;
        let words = (1..(limit - at) as u32 + 80)
            .chain((SEGMENT - at - 24) as u32..(SEGMENT - at + 24) as u32)
            .chain([1 << 20, 0x8000_0000, SEAL_MARKER - 1, SEAL_MARKER, 0]);
        for durable in [false, true] {
            let (mut mapped, mut safe) = (
                open(dir.path(), true, durable),
                open(dir.path(), false, durable),
            );
            for reader in [&mut mapped, &mut safe] {
                reader.seek(target as u64).unwrap();
                for _ in 0..target {
                    reader.next_slot().unwrap();
                }
                if durable {
                    reader.refresh_checkpoint().unwrap();
                }
                assert_eq!(reader.offset, at);
            }
            assert_eq!(
                limit_now(&mapped),
                limit,
                "target {target}, durable {durable}"
            );
            for word in words.clone() {
                file.write_all_at(&word.to_le_bytes(), at).unwrap();
                let (outcome, loads) = record_loads(|| mapped.read_slot_bounded().unwrap());
                let want = safe.read_slot(true).unwrap();
                let case = format!("target {target}, word {word:#x}, durable {durable}");
                assert_eq!(plain(outcome), plain(want), "{case}");
                assert!(loads.iter().all(|&(_, end)| end <= limit), "{case}");
                let end = at + format::align8(u64::from(word));
                if end > limit && word != SEAL_MARKER {
                    assert!(
                        loads.iter().all(|&l| l == (at, at + 4)),
                        "{case}: {loads:?}"
                    );
                    crossed += 1;
                } else if word >= format::RECORD_HEADER_LEN as u32 && word != SEAL_MARKER {
                    assert_eq!(loads.iter().map(|l| l.1).max(), Some(end), "{case}");
                    inside += 1;
                }
                assert_eq!(mapped.buf.capacity(), 0, "{case}");
                // The bound is neither renewed nor spent by a slot in front of it.
                assert_eq!(limit_now(&mapped), limit, "{case}");
            }
            file.write_all_at(&frames[target].1.to_le_bytes(), at)
                .unwrap();
        }
    }
    assert!(crossed > 400 && inside > 800, "{crossed}, {inside}");
}

/// A frame that crosses the bound and passes every check (as a damaged length could by chance): the record is returned
/// from the reader's own buffer, as the safe reader returns it, and the buffer is gone once the next slot is read.
#[test]
fn a_crossing_frame_that_validates_is_returned_from_the_buffer() {
    if !MAPPED_READS {
        return;
    }
    for durable in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(dir.path());
        for _ in 0..8 {
            append(&mut w, 100);
        }
        w.checkpoint().unwrap();
        append(&mut w, 100);
        drop(w);
        let frames = frames(dir.path(), 0);
        let (at, limit) = (frames[5].0, frames[8].0);
        // Record 5 claims everything through record 8, whose first bytes lie after the bound.
        let len = (frames[8].0 + u64::from(frames[8].1) - at) as u32;
        let file = raw(&segment(dir.path(), 0));
        let mut body = vec![0; len as usize - 4];
        file.read_exact_at(&mut body, at + 4).unwrap();
        format::finish_record_body(len, &mut body);
        file.write_all_at(&len.to_le_bytes(), at).unwrap();
        file.write_all_at(&body, at + 4).unwrap();

        let mut reader = open(dir.path(), true, durable);
        for seq in 0..5 {
            assert_eq!(reader.try_next().unwrap().unwrap().seq, seq);
            assert!(!is_copied(&reader));
        }
        assert_eq!(limit_now(&reader), limit);
        let (record, loads) = record_loads(|| {
            let record = reader.try_next().unwrap().unwrap();
            (
                record.seq,
                record.payload.to_vec(),
                record.payload.as_ptr_range(),
            )
        });
        assert!(loads.iter().all(|&l| l == (at, at + 4)), "{loads:?}");
        assert_eq!(record.0, 5);
        assert_eq!(record.1, body[36..]);
        let buf = reader.buf.as_ptr_range();
        assert!(buf.start <= record.2.start && record.2.end <= buf.end);
        assert!(is_copied(&reader));
        assert_eq!(reader.buf.len(), len as usize, "only the claimed frame");
        assert!(reader.try_next().unwrap().is_none());
        assert_eq!(reader.buf.capacity(), 0);
        // The cursor is past the bound (a durable-only reader's too: it counts records, and this one was not one).
        assert_eq!(limit_now(&reader), NO_LIMIT);
        let mut safe = open(dir.path(), false, durable);
        let (want, end) = drain(&mut safe);
        let (got, got_end) = drain(&mut open(dir.path(), true, durable));
        assert_eq!((got, got_end), (want, end));
    }
}

/// Test plan 4: a CRC-valid marker at every aligned offset of an intact journal. Every record is returned, the payload
/// is right while it is borrowed, and exactly the record that the marker cuts is copied.
#[test]
fn a_marker_at_every_offset_costs_at_most_one_copy() {
    if !MAPPED_READS {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut w = writer(dir.path());
    let lens: Vec<usize> = (0..30).map(|i| (i * 97) % 450).collect();
    for &len in &lens {
        append(&mut w, len);
    }
    // A second segment, so that the marker can also be before and after the cursor's segment.
    while w.next_seq() < 30
        || !segment(dir.path(), 400).exists() && frames(dir.path(), 0).len() < 120
    {
        append(&mut w, 500);
        if w.next_seq() > 130 {
            break;
        }
    }
    let total = w.next_seq();
    drop(w);
    let frames = frames(dir.path(), 0);
    let end = frames[29].0 + 1_000;
    let offsets = (0..end)
        .step_by(8)
        .chain((end..SEGMENT + 64).step_by(1_032))
        .chain([4, 60, 65, 100, SEGMENT - 8, SEGMENT, u64::MAX]);
    let mut copies = 0;
    for offset in offsets {
        let valid = crate::status::valid_offset(offset, SEGMENT);
        put_marker(dir.path(), 0, offset, total);
        for durable in [false, true] {
            let mut reader = open(dir.path(), true, durable);
            let mut copied = Vec::new();
            for seq in 0..total {
                let record = reader.try_next().unwrap().unwrap();
                let len = lens.get(seq as usize).copied().unwrap_or(500);
                assert_eq!(record.seq, seq);
                assert_eq!(record.payload, payload(seq, len), "marker {offset}");
                if is_copied(&reader) {
                    copied.push(seq);
                }
            }
            assert!(reader.try_next().unwrap().is_none());
            let cut = frames
                .iter()
                .position(|&(at, len)| at < offset && offset < at + format::align8(u64::from(len)))
                .filter(|_| valid);
            let want: Vec<u64> = cut.into_iter().map(|i| i as u64).collect();
            assert_eq!(copied, want, "marker {offset}, durable {durable}");
            copies += copied.len();
            assert_eq!(reader.buf.capacity(), 0);
        }
    }
    assert!(copies > 1_000, "{copies}");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(if cfg!(miri) { 1 } else { 48 }))]

    /// Test plan 6: with the writer's own markers no record of an intact journal crosses the bound, whatever the
    /// reader has read when they are written; nor with a marker file that is short, has one slot left, names a segment
    /// that does not exist, is gone or can't be read.
    #[test]
    fn an_intact_chain_never_crosses(
        ops in prop::collection::vec(
            prop_oneof![
                6 => (0usize..6_000).prop_map(Some),
                3 => Just(None),
            ],
            1..60,
        ),
        reads in prop::collection::vec(0usize..8, 60),
        damage in 0usize..9,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(dir.path());
        let mut readers = [(false, false), (true, false), (true, true)]
            .map(|(mapped, durable)| open(dir.path(), mapped, durable));
        let mut seen = [0u64; 3];
        let mut lens = Vec::new();
        for (op, read) in ops.iter().zip(reads) {
            match op {
                Some(len) => {
                    append(&mut w, *len);
                    lens.push(*len);
                }
                None => w.checkpoint().unwrap(),
            }
            for (reader, seen) in readers.iter_mut().zip(&mut seen) {
                for _ in 0..read {
                    let Some(record) = reader.try_next().unwrap() else { break };
                    prop_assert_eq!(record.seq, *seen);
                    prop_assert_eq!(record.payload, &payload(*seen, lens[*seen as usize])[..]);
                    prop_assert!(!is_copied(reader));
                    *seen += 1;
                }
            }
        }
        drop(w);
        let path = dir.path().join(crate::journal::CHECKPOINT_FILE);
        let marker = std::fs::read(&path).unwrap_or_default();
        let at = |slot: usize, with: u8| {
            let mut marker = marker.clone();
            if let Some(slot) = marker.get_mut(slot..slot + 64) {
                slot.fill(with);
            }
            marker
        };
        match damage {
            0 => {}
            1 => std::fs::write(&path, &marker[..marker.len().min(64)]).unwrap(),
            2 => std::fs::write(&path, &marker[..marker.len().min(65_536 + 63)]).unwrap(),
            3 => std::fs::write(&path, at(0, 0)).unwrap(),
            4 => std::fs::write(&path, at(65_536, 0xff)).unwrap(),
            5 => put_marker(dir.path(), 1 << 40, 64, 1 << 40),
            6 => put_marker(dir.path(), 1, 64, 1),
            7 => drop(std::fs::remove_file(&path)),
            _ => {
                let _ = std::fs::remove_file(&path);
                std::fs::create_dir(&path).unwrap();
            }
        }
        let mut fresh = open(dir.path(), true, false);
        let mut fresh_seen = 0;
        for (reader, seen) in [(&mut readers[1], &mut seen[1]), (&mut fresh, &mut fresh_seen)] {
            while let Some(record) = reader.try_next().unwrap() {
                prop_assert_eq!(record.seq, *seen);
                prop_assert_eq!(record.payload, &payload(*seen, lens[*seen as usize])[..]);
                prop_assert!(!is_copied(reader));
                *seen += 1;
            }
            prop_assert_eq!(*seen, lens.len() as u64);
        }
        if damage == 0 {
            prop_assert_eq!(drain(&mut readers[2]).1, "Ok(None)");
        }
    }
}

/// Makes a reader that waits at an empty slot do its 100 ms check at the next call.
fn idle(reader: &mut Reader) {
    reader.waiting_since = Instant::now().checked_sub(super::IDENTITY_CHECK_INTERVAL * 2);
    assert!(reader.waiting_since.is_some());
}

/// Test plan 9: when the bound of a visible reader is set, kept, renewed and removed.
#[test]
fn the_limit_follows_the_marker() {
    if !MAPPED_READS {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut w = writer(dir.path());
    let frame = 144; // 100 bytes of payload.
    let after = |records: u64| FIRST + records * frame;
    let mut safe = open(dir.path(), false, false);
    let mut early = open(dir.path(), true, false);
    assert_eq!(limit_now(&early), NO_LIMIT, "no marker");
    for _ in 0..10 {
        append(&mut w, 100);
    }
    w.checkpoint().unwrap();
    let mut reader = open(dir.path(), true, false);
    assert_eq!(limit_now(&reader), after(10));
    for _ in 0..10 {
        append(&mut w, 100);
    }
    w.checkpoint().unwrap();
    // Below the bound: not read again.
    for _ in 0..10 {
        reader.try_next().unwrap().unwrap();
        assert_eq!(limit_now(&reader), after(10));
    }
    // At it, with the marker ahead: renewed, and the record is inside the new bound.
    reader.try_next().unwrap().unwrap();
    assert_eq!(limit_now(&reader), after(20));
    for _ in 0..9 {
        reader.try_next().unwrap().unwrap();
    }
    for _ in 0..5 {
        append(&mut w, 100);
    }
    // At it, with the marker there: the bound is gone, and the marker is not read for the next records.
    reader.try_next().unwrap().unwrap();
    assert_eq!(limit_now(&reader), NO_LIMIT);
    w.checkpoint().unwrap();
    for _ in 0..4 {
        reader.try_next().unwrap().unwrap();
        assert_eq!(limit_now(&reader), NO_LIMIT);
    }
    // At an empty slot: the marker is here. One that is ahead (it would cover records that are not there) bounds.
    assert!(reader.try_next().unwrap().is_none());
    idle(&mut reader);
    assert!(reader.try_next().unwrap().is_none());
    assert_eq!(limit_now(&reader), NO_LIMIT);
    put_marker(dir.path(), 0, after(30), 30);
    assert!(reader.try_next().unwrap().is_none());
    assert_eq!(limit_now(&reader), NO_LIMIT, "not before 100 ms");
    idle(&mut reader);
    assert!(reader.try_next().unwrap().is_none());
    assert_eq!(limit_now(&reader), after(30));
    // It can't be read: the old bound stays, and nothing is reported. No valid marker: none.
    let path = dir.path().join(crate::journal::CHECKPOINT_FILE);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    idle(&mut reader);
    assert!(reader.try_next().unwrap().is_none());
    assert_eq!(limit_now(&reader), after(30));
    std::fs::remove_dir(&path).unwrap();
    idle(&mut reader);
    assert!(reader.try_next().unwrap().is_none());
    assert_eq!(limit_now(&reader), NO_LIMIT);
    // A bound that ends at the empty slot itself is reached there.
    put_marker(dir.path(), 0, after(25), 25);
    reader.seek(0).unwrap();
    assert_eq!(limit_now(&reader), after(25));
    while reader.try_next().unwrap().is_some() {}
    assert_eq!(limit_now(&reader), NO_LIMIT);
    // The next segment: by the marker read before it is opened.
    let base = {
        while !segment(dir.path(), w.next_seq() - 1).exists() || w.next_seq() < 30 {
            append(&mut w, 100);
        }
        w.next_seq() - 1
    };
    while reader.position() < base {
        reader.try_next().unwrap().unwrap();
    }
    put_marker(dir.path(), base, after(1), base + 1);
    reader.try_next().unwrap().unwrap();
    assert_eq!((reader.seg.base, limit_now(&reader)), (base, after(1)));
    // A segment before the marker's is bounded by its size, as every segment is.
    reader.seek(3).unwrap();
    assert_eq!((reader.seg.base, limit_now(&reader)), (0, NO_LIMIT));
    while reader.position() < base {
        reader.try_next().unwrap().unwrap();
        assert_eq!(limit_now(&reader), NO_LIMIT);
    }
    reader.try_next().unwrap().unwrap();
    assert_eq!((reader.seg.base, limit_now(&reader)), (base, after(1)));
    // One that names an earlier segment, or an offset that is no slot, bounds nothing.
    for (marker_base, offset) in [(0, after(3)), (base, 60), (base, 68), (base, SEGMENT)] {
        put_marker(dir.path(), marker_base, offset, 3);
        reader.seek(base).unwrap();
        assert_eq!(limit_now(&reader), NO_LIMIT);
    }
    put_marker(dir.path(), base, FIRST, base);
    reader.seek(base).unwrap();
    assert_eq!(limit_now(&reader), NO_LIMIT, "at the cursor already");
    // `seek_to_end` forgets a bound that was ahead.
    put_marker(dir.path(), base, 40_000, 1 << 20);
    reader.seek(base).unwrap();
    assert_eq!(limit_now(&reader), 40_000);
    put_marker(dir.path(), base, after(1), base + 1);
    reader.seek_to_end().unwrap();
    assert_eq!(limit_now(&reader), NO_LIMIT);
    // The other readers.
    assert_eq!(drain(&mut early), drain(&mut safe));
    assert_eq!(limit_now(&safe), NO_LIMIT);
}

/// The bound follows the size of the segment the reader is in, for every kind of reader (a writer opened with another
/// segment size rolls to segments of that size).
#[test]
fn segments_of_different_sizes() {
    let dir = tempfile::tempdir().unwrap();
    for size in [SEGMENT, 1 << 18, SEGMENT] {
        let config = WriterConfig::default().segment_size(size);
        let mut w = Writer::open(dir.path(), config).unwrap();
        for _ in 0..400 {
            append(&mut w, 1_000);
        }
        w.checkpoint().unwrap();
    }
    for (mapped, durable) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut reader = open(dir.path(), mapped, durable);
        for seek in [0, 700, 300] {
            reader.seek(seek).unwrap();
            let (records, end) = drain(&mut reader);
            assert_eq!(
                (records.len() as u64, end.as_str()),
                (1_200 - seek, "Ok(None)")
            );
            assert_eq!(reader.bound, reader.seg.size);
        }
    }
}

/// Seeks: a candidate segment takes the bound of its open, a seek that continues keeps the bound or takes the one of the
/// marker it reads, `seek_to_end` starts at the marker, and a failed one restores the bound.
#[test]
fn seeks_set_keep_and_restore_the_limit() {
    if !MAPPED_READS {
        return;
    }
    let after = |records: u64| FIRST + records * 144;
    for durable in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(dir.path());
        for _ in 0..20 {
            append(&mut w, 100);
        }
        w.checkpoint().unwrap();
        for _ in 0..20 {
            append(&mut w, 100);
        }
        let mut reader = open(dir.path(), true, durable);
        reader.seek(0).unwrap();
        let first = if durable { NO_LIMIT } else { after(20) };
        assert_eq!(limit_now(&reader), first, "durable {durable}");
        reader.try_next().unwrap().unwrap();
        assert_eq!(limit_now(&reader), after(20));
        w.checkpoint().unwrap();
        let at = w.next_seq();
        // Continues, to a target the known marker covers: nothing is read, and the bound stays.
        reader.seek(10).unwrap();
        assert_eq!((reader.offset, limit_now(&reader)), (after(1), after(20)));
        reader.try_next().unwrap().unwrap();
        // Continues, past it: a visible reader takes the marker read there.
        let target = at.max(25);
        reader.seek(target).unwrap();
        assert_eq!(reader.offset, after(11));
        assert_eq!(
            limit_now(&reader),
            if durable { after(20) } else { after(at) }
        );
        // The end.
        for _ in 0..3 {
            append(&mut w, 100);
        }
        reader.seek_to_end().unwrap();
        assert_eq!(reader.offset, after(if durable { at } else { at + 3 }));
        assert_eq!(limit_now(&reader), NO_LIMIT);
        assert_eq!(reader.position(), if durable { at } else { at + 3 });
        // A failed one, and then a record past the marker that can't be read.
        reader.seek(0).unwrap();
        reader.try_next().unwrap().unwrap();
        let (limit, offset) = (limit_now(&reader), reader.durable_offset);
        assert_eq!(limit, after(at));
        assert_eq!(offset, if durable { after(at) } else { 0 });
        let file = raw(&segment(dir.path(), 0));
        if !durable {
            file.write_all_at(&[0x5a], after(at) + 60).unwrap();
            assert!(matches!(reader.seek_to_end(), Err(Error::Corrupt { .. })));
            let byte = payload(at, 100)[20];
            file.write_all_at(&[byte], after(at) + 60).unwrap();
        } else {
            let path = dir.path().join(crate::journal::CHECKPOINT_FILE);
            let saved = dir.path().join("saved");
            std::fs::rename(&path, &saved).unwrap();
            std::fs::create_dir(&path).unwrap();
            assert!(matches!(reader.seek_to_end(), Err(Error::Io { .. })));
            std::fs::remove_dir(&path).unwrap();
            std::fs::rename(&saved, &path).unwrap();
        }
        assert_eq!((limit_now(&reader), reader.durable_offset), (limit, offset));
        assert_eq!(reader.try_next().unwrap().unwrap().seq, 1);
    }
}

/// A durable-only reader's bound is the marker it has applied, in its own segment and in the next one.
#[test]
fn a_durable_only_limit_is_the_applied_marker() {
    if !MAPPED_READS {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut w = writer(dir.path());
    let after = |records: u64| FIRST + records * 144;
    for _ in 0..5 {
        append(&mut w, 100);
    }
    w.checkpoint().unwrap();
    let mut reader = open(dir.path(), true, true);
    assert_eq!(limit_now(&reader), NO_LIMIT);
    for _ in 0..5 {
        reader.try_next().unwrap().unwrap();
        assert_eq!(limit_now(&reader), after(5));
    }
    append(&mut w, 100);
    assert!(reader.try_next().unwrap().is_none());
    assert_eq!(limit_now(&reader), NO_LIMIT);
    while !segment(dir.path(), w.next_seq() - 1).exists() || w.next_seq() < 10 {
        append(&mut w, 100);
    }
    let base = w.next_seq() - 1;
    append(&mut w, 100);
    w.checkpoint().unwrap();
    append(&mut w, 100);
    while reader.position() < base {
        reader.try_next().unwrap().unwrap();
        assert_eq!(limit_now(&reader), NO_LIMIT);
    }
    reader.try_next().unwrap().unwrap();
    assert_eq!((reader.seg.base, limit_now(&reader)), (base, after(2)));
    // A marker that does not move past the cursor is applied as it always was. Lost: nothing is covered or bounded.
    reader.try_next().unwrap().unwrap();
    std::fs::remove_file(dir.path().join(crate::journal::CHECKPOINT_FILE)).unwrap();
    assert!(reader.try_next().unwrap().is_none());
    assert_eq!((limit_now(&reader), reader.durable_offset), (NO_LIMIT, 0));
}

/// Test plan 5, the load side: under a live writer that appends continuously, a visible and a durable-only reader meet
/// a covered length that claims 20 frames, 10 of them past the writer's position; it survived `Writer::open`. No load
/// ends after the bound, every time the reader tries again, and the error is the safe reader's.
#[test]
fn a_covered_enlarged_length_under_a_live_writer() {
    if !MAPPED_READS {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut w = writer(dir.path());
    for _ in 0..30 {
        append(&mut w, 100);
    }
    w.checkpoint().unwrap();
    drop(w);
    let file = raw(&segment(dir.path(), 0));
    let at = FIRST + 20 * 144;
    file.write_all_at(&(20u32 * 144).to_le_bytes(), at).unwrap();
    let mut w = writer(dir.path());
    assert!(
        w.recovery_report().actions.is_empty(),
        "{:?}",
        w.recovery_report()
    );
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        let stop = &stop;
        let readers: Vec<_> = [(false, false), (true, false), (false, true), (true, true)]
            .into_iter()
            .map(|(mapped, durable)| {
                let dir = dir.path();
                s.spawn(move || {
                    let mut reader = open(dir, mapped, durable);
                    for _ in 0..20 {
                        reader.try_next().unwrap().unwrap();
                    }
                    assert_eq!(
                        limit_now(&reader),
                        if mapped { FIRST + 30 * 144 } else { NO_LIMIT }
                    );
                    let (errors, loads) = record_loads(|| {
                        [(); 3].map(|()| match reader.try_next() {
                            Err(Error::Corrupt { offset, reason, .. }) => {
                                (offset, format!("{reason:?}"))
                            }
                            other => panic!("{other:?}"),
                        })
                    });
                    assert!(
                        loads.iter().all(|&(from, to)| (from, to) == (at, at + 4)),
                        "{loads:?}"
                    );
                    assert_eq!(loads.is_empty(), !mapped);
                    let bad = (at, format!("{:?}", CorruptReason::BadChecksum));
                    assert_eq!(errors, [(); 3].map(|()| bad.clone()));
                })
            })
            .collect();
        let writer = s.spawn(move || {
            // One segment's worth, over and over: more than the frame reaches.
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                append(&mut w, 100);
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        });
        let results: Vec<_> = readers.into_iter().map(|r| r.join()).collect();
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        writer.join().unwrap();
        for result in results {
            result.unwrap();
        }
    });
}

/// Test plan 8 (C14), with the failures in the marker read and in the identity check that only a fault point can make:
/// a visible reader overtaken by retention returns every record of its segment, then `Discarded`.
#[test]
#[cfg(feature = "fault-injection")]
fn a_reader_overtaken_by_retention_reads_its_segment_to_the_end() {
    use crate::fault::{self, Point};
    use crate::retention::{self, Action, Policy};
    for case in [
        "plain",
        "no marker",
        "unreadable marker",
        "marker fault",
        "stat fault",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(dir.path());
        for _ in 0..10 {
            append(&mut w, 100);
        }
        w.checkpoint().unwrap();
        let mut reader = open(dir.path(), true, false);
        reader.try_next().unwrap().unwrap();
        while !segment(dir.path(), w.next_seq() - 1).exists() || w.next_seq() < 20 {
            append(&mut w, 100);
        }
        let base = w.next_seq() - 1;
        for _ in 0..1_000 {
            append(&mut w, 100);
        }
        w.checkpoint().unwrap();
        let plan = retention::plan(dir.path(), &Policy::default().max_segments(1)).unwrap();
        retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
        assert!(!segment(dir.path(), 0).exists());
        let path = dir.path().join(crate::journal::CHECKPOINT_FILE);
        fault::clear();
        match case {
            "no marker" => std::fs::remove_file(&path).unwrap(),
            "unreadable marker" => {
                std::fs::remove_file(&path).unwrap();
                std::fs::create_dir(&path).unwrap();
            }
            "marker fault" => fault::fail_next(Point::ReaderLimitMarker, libc::EIO),
            "stat fault" => fault::fail_next(Point::ReaderLimitIdentity, libc::EACCES),
            _ => {}
        }
        for seq in 1..base {
            let record = reader.try_next().unwrap().expect(case);
            assert_eq!((record.seq, record.payload), (seq, &payload(seq, 100)[..]));
            assert!(!is_copied(&reader));
        }
        assert!(
            matches!(reader.try_next(), Err(Error::Discarded { seq, .. }) if seq == base),
            "{case}"
        );
        fault::clear();
    }
}

/// The marker is read again once when the bound is reached, not for every record after it, and a check that fails there
/// uses that one read.
#[test]
#[cfg(feature = "fault-injection")]
fn the_marker_is_read_once_at_the_limit() {
    use crate::fault::{self, Point};
    use std::cell::Cell;
    use std::rc::Rc;
    if !MAPPED_READS {
        return;
    }
    for fail in [
        None,
        Some(Point::ReaderLimitMarker),
        Some(Point::ReaderLimitIdentity),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer(dir.path());
        for _ in 0..10 {
            append(&mut w, 100);
        }
        w.checkpoint().unwrap();
        let mut reader = open(dir.path(), true, false);
        for _ in 0..10 {
            reader.try_next().unwrap().unwrap();
        }
        for _ in 0..50 {
            append(&mut w, 100);
        }
        if fail.is_some() {
            // Even if a marker covers them all.
            w.checkpoint().unwrap();
        }
        fault::clear();
        let reads = Rc::new(Cell::new(0));
        for _ in 0..10 {
            let reads = reads.clone();
            fault::run_at(Point::ReaderLimitMarker, 0, move || {
                reads.set(reads.get() + 1)
            });
        }
        if let Some(point) = fail {
            fault::fail_next(point, libc::EIO);
        }
        for _ in 0..50 {
            reader.try_next().unwrap().unwrap();
            assert_eq!((reads.get(), limit_now(&reader)), (1, NO_LIMIT), "{fail:?}");
        }
        fault::clear();
    }
}

/// A repair at each point of a re-read at the bound: the reader returns what it would have returned without the bound,
/// and takes the marker of the new history only if its file is still the current one.
#[test]
#[cfg(feature = "fault-injection")]
fn a_repair_around_the_marker_read() {
    use crate::fault::{self, Point};
    if !MAPPED_READS {
        return;
    }
    for point in [Point::ReaderLimitMarker, Point::ReaderLimitIdentity] {
        for replace in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut w = writer(dir.path());
            for _ in 0..10 {
                append(&mut w, 100);
            }
            w.checkpoint().unwrap();
            append(&mut w, 100);
            append(&mut w, 100);
            drop(w);
            let mut reader = open(dir.path(), true, false);
            for _ in 0..10 {
                reader.try_next().unwrap().unwrap();
            }
            let path = dir.path().to_path_buf();
            fault::clear();
            fault::run_at(point, 0, move || {
                // The record at the bound and the one after it are lost, and a new history is written and covered.
                let at = FIRST + 10 * 144;
                let file = raw(&segment(&path, 0));
                if replace {
                    let mut image = std::fs::read(segment(&path, 0)).unwrap();
                    image[at as usize + 20] ^= 0xff;
                    std::fs::write(path.join("copy"), image).unwrap();
                    std::fs::rename(path.join("copy"), segment(&path, 0)).unwrap();
                } else {
                    file.write_all_at(&[0; 4], at).unwrap();
                    file.write_all_at(&[0; 4], at + 144).unwrap();
                }
                let mut w = writer(&path);
                assert_eq!(w.next_seq(), 10);
                for _ in 0..4 {
                    append(&mut w, 60);
                }
                w.checkpoint().unwrap();
            });
            let case = format!("{point:?}, replace {replace}");
            if replace {
                // The old file, unchanged, until the reader notices.
                for seq in 10..12 {
                    let record = reader.try_next().unwrap().expect(&case);
                    assert_eq!((record.seq, record.payload), (seq, &payload(seq, 100)[..]));
                }
                assert_eq!(limit_now(&reader), NO_LIMIT, "{case}: not for this file");
                assert!(reader.try_next().unwrap().is_none());
                idle(&mut reader);
                assert!(matches!(
                    reader.try_next(),
                    Err(Error::Discarded { seq: 10 | 12, .. })
                ));
            } else {
                // The commit word was read before the repair; the slot is read again, and is the new history's.
                for seq in 10..14 {
                    let record = reader.try_next().unwrap().expect(&case);
                    assert_eq!((record.seq, record.payload), (seq, &payload(seq, 60)[..]));
                }
                assert!(reader.try_next().unwrap().is_none());
            }
            fault::clear();
        }
    }
}

/// A landing through a seek index sets the bound to the segment's end (its marker names a later segment); a durable-only
/// reader keeps the bound of the marker it has applied.
#[test]
#[cfg(feature = "fault-injection")]
fn an_index_landing_sets_the_limit() {
    if !MAPPED_READS {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), WriterConfig::default().segment_size(1 << 20)).unwrap();
    append(&mut w, 100);
    w.checkpoint().unwrap();
    let mut visible = open_with(dir.path(), true, ReaderConfig::default().seek_index(true));
    let config = ReaderConfig::default().seek_index(true).durable_only(true);
    let mut durable = open_with(dir.path(), true, config);
    for reader in [&mut visible, &mut durable] {
        reader.try_next().unwrap().unwrap();
        assert_eq!(limit_now(reader), FIRST + 144);
    }
    for _ in 0..10_000 {
        append(&mut w, 100);
    }
    w.checkpoint().unwrap();
    crate::seek_index::build(dir.path(), false).unwrap();
    for (reader, limit) in [(&mut visible, NO_LIMIT), (&mut durable, FIRST + 144)] {
        // The marker that the segment's open reads is not the one that the landing needs.
        put_marker(dir.path(), 0, FIRST + 144, 1);
        let path = dir.path().to_path_buf();
        crate::fault::clear();
        crate::fault::run_at(crate::fault::Point::ReaderOpen, 0, move || {
            put_marker(&path, 9_999, 64, 9_999);
        });
        reader.seek(6_000).unwrap();
        assert!(reader.used_index);
        assert!(reader.offset > FIRST + 144);
        assert!(limit_now(reader) == limit, "{}", limit_now(reader));
        assert_eq!(reader.try_next().unwrap().unwrap().seq, 6_000);
        assert_eq!(limit_now(reader), NO_LIMIT);
        assert_eq!(reader.buf.capacity(), 0);
    }
}
