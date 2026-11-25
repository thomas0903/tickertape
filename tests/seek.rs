//! `Reader::seek` and `Reader::seek_to_end` (ADR 0046): a forward seek that continues from the cursor lands exactly
//! where one that scans the segment from its start lands, and continues only where the cursor is known to be on the
//! current chain; the end of the journal is found without reading the segments before it.

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use tickertape::fault::Point;
use tickertape::retention::{self, Action, Policy};
use tickertape::{
    CorruptReason, Error, NewRecord, OwnedRecord, Reader, ReaderConfig, RecoveryAction, Writer,
    format,
};
use tickertape_tests::{
    append_n, check_record, corrupt_by_replacement, locate_record, open_raw, segments,
    small_config, zero_commit_words_from,
};

const FIRST_SLOT: u64 = format::SEGMENT_HEADER_LEN as u64;

/// The kinds of reader: (mapped, durable-only).
const KINDS: [(bool, bool); 4] = [(false, false), (true, false), (false, true), (true, true)];

fn open(dir: &Path, (mapped, durable): (bool, bool)) -> Reader {
    let config = ReaderConfig::default().durable_only(durable).notify(false);
    if mapped {
        // SAFETY: only tickertape writers modify the segment files in these tests, except the tests' own zeroing of
        // commit words, while no reader holds a payload.
        unsafe { Reader::open_mapped(dir, config) }.unwrap()
    } else {
        Reader::open(dir, config).unwrap()
    }
}

/// A reader that has sought to the oldest record, and so knows what the checkpoint covers now.
fn open_sought(dir: &Path, kind: (bool, bool)) -> Reader {
    let mut reader = open(dir, kind);
    reader.seek(segments(dir)[0].0).unwrap();
    assert_eq!(offset(&reader), FIRST_SLOT);
    reader
}

/// The offset the reader is at.
fn offset(reader: &Reader) -> u64 {
    reader.cursor().position.offset
}

/// Reads `n` records, checking that they are `from..from + n` as [`append_n`] wrote them.
fn expect(reader: &mut Reader, from: u64, n: u64) {
    for seq in from..from + n {
        let record = reader.next_owned().unwrap().expect("a committed record");
        assert_eq!(record.seq, seq);
        check_record(&record);
    }
}

fn append_tagged(writer: &mut Writer, n: u64, tag: u32, len: usize) {
    for _ in 0..n {
        writer
            .append(&NewRecord {
                schema_id: tag,
                venue_ts_ns: writer.next_seq(),
                local_ts_ns: 0,
                payload: &vec![tag as u8; len],
            })
            .unwrap();
    }
}

/// Up to `limit` records from the cursor, and how reading ended (`None`: the limit; `Some("end")`: nothing more to
/// read; otherwise the error).
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

#[test]
fn a_covered_forward_seek_continues_from_the_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 100);
    writer.checkpoint().unwrap();
    assert_eq!(segments(dir.path()).len(), 1);
    for kind in KINDS {
        let mut reader = open_sought(dir.path(), kind);
        expect(&mut reader, 0, 10);
        let at = offset(&reader);
        assert!(at > FIRST_SLOT);
        // Forward, to where it is, and to the record before the last.
        for target in [10, 40, 98] {
            let before = offset(&reader);
            reader.seek(target).unwrap();
            assert_eq!(offset(&reader), before, "{kind:?}: target {target}");
            assert_eq!(reader.position(), target);
            expect(&mut reader, target, 1);
        }
        // A seek ahead that the reader has not caught up with yet, then one to a record before it but still ahead.
        reader.seek(5).unwrap();
        assert_eq!(offset(&reader), FIRST_SLOT, "backward: {kind:?}");
        expect(&mut reader, 5, 1);
        let before = offset(&reader);
        reader.seek(90).unwrap();
        reader.seek(50).unwrap();
        assert_eq!(offset(&reader), before, "{kind:?}");
        expect(&mut reader, 50, 1);
        // Not written yet, then written.
        reader.seek(100).unwrap();
        assert!(reader.try_next().unwrap().is_none());
        reader.seek(100).unwrap();
        assert!(offset(&reader) > FIRST_SLOT, "{kind:?}");
        assert!(reader.try_next().unwrap().is_none());
    }
    let mut readers = KINDS.map(|kind| open(dir.path(), kind));
    for reader in &mut readers {
        expect(reader, 0, 100);
        reader.seek(104).unwrap();
        assert!(reader.try_next().unwrap().is_none());
    }
    append_n(&mut writer, 5);
    writer.checkpoint().unwrap();
    for reader in &mut readers {
        expect(reader, 104, 1);
    }
}

#[test]
fn seeks_across_segments_start_at_the_segment() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 2_000);
    writer.checkpoint().unwrap();
    let bases: Vec<u64> = segments(dir.path()).iter().map(|s| s.0).collect();
    assert!(bases.len() > 3);
    for kind in KINDS {
        let mut reader = open_sought(dir.path(), kind);
        // In a sealed segment, up to its last record and then at its seal: the next record is in the successor.
        expect(&mut reader, 0, 3);
        reader.seek(bases[1] - 1).unwrap();
        assert!(offset(&reader) > FIRST_SLOT, "{kind:?}");
        expect(&mut reader, bases[1] - 1, 1);
        reader.seek(bases[1]).unwrap();
        assert_eq!(offset(&reader), FIRST_SLOT, "{kind:?}");
        expect(&mut reader, bases[1], 2);
        reader.seek(bases[2] + 4).unwrap();
        assert_eq!(offset(&reader), FIRST_SLOT, "{kind:?}");
        expect(&mut reader, bases[2] + 4, 1);
        // After following a seal while skipping.
        reader.seek(bases[2] + 6).unwrap();
        assert!(offset(&reader) > FIRST_SLOT, "{kind:?}");
        expect(&mut reader, bases[2] + 6, 2_000 - bases[2] - 6);
        assert!(reader.try_next().unwrap().is_none());
    }
}

/// Only a checkpoint makes the records behind the cursor ones that recovery never removes: without coverage of the
/// cursor, established before the reader read them, a seek scans the segment from its start, as it always did.
#[test]
fn an_uncovered_forward_seek_scans_from_the_segments_start() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 100);
    let marker = dir.path().join("checkpoint");
    for mapped in [false, true] {
        let mut reader = open_sought(dir.path(), (mapped, false));
        expect(&mut reader, 0, 10);
        reader.seek(40).unwrap();
        assert_eq!(offset(&reader), FIRST_SLOT, "no checkpoint");
        expect(&mut reader, 40, 1);
    }
    writer.checkpoint().unwrap();
    append_n(&mut writer, 100);
    for kind in KINDS {
        // The first seek scans: what the reader read before it knew a marker is not trusted, covered or not.
        let mut reader = open(dir.path(), kind);
        expect(&mut reader, 0, 10);
        reader.seek(20).unwrap();
        assert_eq!(offset(&reader), FIRST_SLOT, "{kind:?}: first seek");
        expect(&mut reader, 20, 1);
        let at = offset(&reader);
        reader.seek(30).unwrap();
        assert_eq!(offset(&reader), at, "{kind:?}: second seek");
    }
    for mapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut writer, 100);
        writer.checkpoint().unwrap();
        append_n(&mut writer, 100);
        let mut reader = open_sought(dir.path(), (mapped, false));
        expect(&mut reader, 0, 100);
        let at = offset(&reader);
        reader.seek(100).unwrap();
        assert_eq!(offset(&reader), at, "covered exactly up to the cursor");
        expect(&mut reader, 100, 1);
        reader.seek(110).unwrap();
        assert_eq!(offset(&reader), FIRST_SLOT, "one past the checkpoint");
        expect(&mut reader, 110, 1);
        // A checkpoint made after the reader passed records does not make it trust them.
        writer.checkpoint().unwrap();
        reader.seek(120).unwrap();
        assert_eq!(offset(&reader), FIRST_SLOT, "read before the checkpoint");
        expect(&mut reader, 120, 1);
        reader.seek(130).unwrap();
        assert!(offset(&reader) > FIRST_SLOT, "read after it");
        // A seek past the known coverage reads the marker again, so that later ones continue past it too.
        let next = writer.next_seq();
        append_n(&mut writer, 20);
        writer.checkpoint().unwrap();
        reader.seek(next).unwrap();
        expect(&mut reader, next, 5);
        let at = offset(&reader);
        reader.seek(next + 10).unwrap();
        assert_eq!(offset(&reader), at, "the coverage is extended");
        expect(&mut reader, next + 10, 1);
    }
    // An unreadable or missing marker covers nothing, and the seek still succeeds.
    for action in [0, 1, 2] {
        match action {
            0 => std::fs::write(&marker, b"invalid marker").unwrap(),
            1 => std::fs::remove_file(&marker).unwrap(),
            _ => std::fs::create_dir(&marker).unwrap(),
        }
        let mut reader = open_sought(dir.path(), (false, false));
        expect(&mut reader, 0, 10);
        reader.seek(40).unwrap();
        assert_eq!(offset(&reader), FIRST_SLOT, "{action}");
        expect(&mut reader, 40, 1);
    }
}

/// Every seek that scans reads the marker again: what an earlier one knew is not carried to another file. Here another
/// journal, without a checkpoint, takes the place of the one the reader knew the coverage of.
#[test]
fn coverage_is_not_carried_to_another_file() {
    let (dir, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 100);
    writer.checkpoint().unwrap();
    drop(writer);
    append_n(
        &mut Writer::open(other.path(), small_config()).unwrap(),
        100,
    );
    let mut reader = open_sought(dir.path(), (false, false));
    expect(&mut reader, 0, 10);
    for name in [format::segment_file_name(0).as_str(), "checkpoint"] {
        std::fs::rename(other.path().join(name), dir.path().join(name)).unwrap();
    }
    reader.seek(20).unwrap();
    assert_eq!(offset(&reader), FIRST_SLOT, "another file");
    expect(&mut reader, 20, 1);
    reader.seek(30).unwrap();
    assert_eq!(offset(&reader), FIRST_SLOT, "nothing is covered in it");
    expect(&mut reader, 30, 1);
}

/// The segment is replaced between the open of a seek that scans and its check that the segment is still in place:
/// the marker it read then says nothing about the file it holds, so the next seek scans again.
#[test]
fn coverage_is_not_applied_to_a_file_that_is_no_longer_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 100);
    writer.checkpoint().unwrap();
    for mapped in [false, true] {
        let mut reader = open(dir.path(), (mapped, false));
        let path = segments(dir.path())[0].1.clone();
        let hidden = dir.path().join("hidden");
        // The file the reader opens is not at the path when it checks; it is again afterwards.
        let copy = dir.path().join("copy");
        std::fs::copy(&path, &copy).unwrap();
        tickertape::fault::clear();
        let (p, h) = (path.clone(), hidden.clone());
        tickertape::fault::run_at(Point::ReaderSeekCoverage, 0, move || {
            std::fs::rename(&p, &h).unwrap();
            std::fs::rename(&copy, &p).unwrap();
        });
        reader.seek(0).unwrap();
        std::fs::rename(&hidden, &path).unwrap();
        expect(&mut reader, 0, 10);
        reader.seek(40).unwrap();
        assert_eq!(offset(&reader), FIRST_SLOT);
        expect(&mut reader, 40, 1);
    }
}

/// Recovery repairs a segment in place (the file keeps its identity) and a new history is appended and checkpointed
/// past the reader's cursor: the marker covers the cursor's sequence number, but records of the new history start at
/// other offsets. `seek` to the reader's position puts it back on the current chain (crash contract C11, "Readers
/// after a repair"), so it must not trust the cursor. With `embed`, the new history's first record holds, in its
/// payload, the old records' bytes at their old offsets, so that the bytes before the cursor still form the record
/// the reader passed last.
#[test]
fn a_seek_after_an_in_place_repair_lands_on_the_current_chain() {
    for kind in KINDS {
        for (len, embed, sought) in [
            (64, false, true),
            (200, false, true),
            (32, false, true),
            (64, true, true),
            (64, true, false),
        ] {
            let case = format!("{kind:?}, {len}, embed {embed}, sought {sought}");
            let dir = tempfile::tempdir().unwrap();
            let mut writer = Writer::open(dir.path(), small_config()).unwrap();
            append_tagged(&mut writer, 10, 1, 64);
            writer.checkpoint().unwrap();
            append_tagged(&mut writer, 8, 1, 64);
            drop(writer);
            let mut reader = if sought {
                open_sought(dir.path(), kind)
            } else {
                open(dir.path(), kind)
            };
            let visible = read_up_to(&mut reader, 18).0.len() as u64;
            assert_eq!(visible, if kind.1 { 10 } else { 18 });
            let (path, at) = locate_record(dir.path(), 13);
            let image = std::fs::read(path).unwrap();
            let end = at as usize + 5 * format::align8(40 + 64) as usize;

            zero_commit_words_from(dir.path(), 13);
            let mut writer = Writer::open(dir.path(), small_config()).unwrap();
            assert!(matches!(
                writer.recovery_report().actions[..],
                [RecoveryAction::ZeroedUncommitted { .. }]
            ));
            if embed {
                let mut payload = image[at as usize + format::RECORD_HEADER_LEN..end].to_vec();
                payload.extend_from_slice(&[0; 100]);
                writer
                    .append(&NewRecord {
                        schema_id: 2,
                        venue_ts_ns: 0,
                        local_ts_ns: 0,
                        payload: &payload,
                    })
                    .unwrap();
            }
            while writer.next_seq() < 33 {
                append_tagged(&mut writer, 1, 2, len);
            }
            writer.checkpoint().unwrap();

            let at = offset(&reader);
            reader.seek(visible).unwrap();
            // A durable-only reader's cursor was covered when it got there; a visible one's was not.
            let continues = kind.1 && sought;
            assert_eq!(
                offset(&reader),
                if continues { at } else { FIRST_SLOT },
                "{case}"
            );
            expect_histories(&mut reader, visible, 33, &case);
        }
    }
}

/// The rest of the records are `from..to`, of history 1 before record 13 and of history 2 from there on.
fn expect_histories(reader: &mut Reader, from: u64, to: u64, case: &str) {
    let (records, end) = read_up_to(reader, 100);
    assert_eq!(end.as_deref(), Some("end"), "{case}");
    let got: Vec<(u64, u32)> = records.iter().map(|r| (r.seq, r.schema_id)).collect();
    let want: Vec<(u64, u32)> = (from..to)
        .map(|seq| (seq, if seq < 13 { 1 } else { 2 }))
        .collect();
    assert_eq!(got, want, "{case}");
}

/// Damage that also destroys the checkpoint marker lets recovery remove *covered* records in place. The seek still
/// notices, because the record before the cursor is not there any more, unless the new history put one there.
#[test]
fn a_seek_after_covered_records_were_removed_in_place_lands_on_the_current_chain() {
    for kind in KINDS {
        // `last`: the length of the new record before the cursor, which starts where the old one did if `len` fits.
        for (len, last, continues) in [
            (64, 64, true),
            (60, 60, true),
            (200, 200, false),
            (32, 32, false),
            (64, 200, false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut writer = Writer::open(dir.path(), small_config()).unwrap();
            append_tagged(&mut writer, 20, 1, 64);
            writer.checkpoint().unwrap();
            drop(writer);
            let mut reader = open_sought(dir.path(), kind);
            assert_eq!(read_up_to(&mut reader, 18).0.len(), 18);

            std::fs::remove_file(dir.path().join("checkpoint")).unwrap();
            zero_commit_words_from(dir.path(), 13);
            let mut writer = Writer::open(dir.path(), small_config()).unwrap();
            assert_eq!(writer.next_seq(), 13);
            append_tagged(&mut writer, 4, 2, len);
            append_tagged(&mut writer, 16, 2, last);
            writer.checkpoint().unwrap();
            let at = offset(&reader);
            reader.seek(18).unwrap();
            let case = format!("{kind:?}, {len}, {last}");
            assert_eq!(
                offset(&reader),
                if continues { at } else { FIRST_SLOT },
                "{case}"
            );
            expect_histories(&mut reader, 18, 33, &case);
        }
    }
}

/// A repair that replaces the file: the reader's open file is not the segment any more, so nothing of it is used, even
/// though the marker covers the cursor and the old file still holds the record before it.
#[test]
fn a_seek_after_a_replacement_reads_the_new_file() {
    for kind in KINDS {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_tagged(&mut writer, 10, 1, 64);
        writer.checkpoint().unwrap();
        append_tagged(&mut writer, 10, 1, 64);
        drop(writer);
        let mut reader = open_sought(dir.path(), kind);
        // Every reader stops where it is covered, so that only the file's identity keeps it from continuing.
        let visible = read_up_to(&mut reader, 10).0.len() as u64;
        corrupt_by_replacement(dir.path(), 13);
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_tagged(&mut writer, 20, 2, 64);
        writer.checkpoint().unwrap();
        reader.seek(visible).unwrap();
        assert_eq!(offset(&reader), FIRST_SLOT, "{kind:?}");
        let (records, end) = read_up_to(&mut reader, 100);
        assert_eq!(end.as_deref(), Some("end"));
        assert!(records.iter().map(|r| r.seq).eq(visible..33));
        assert!(
            records
                .iter()
                .all(|r| r.schema_id == if r.seq < 13 { 1 } else { 2 })
        );
    }
}

/// Every record a continued seek passes is validated: damage between the cursor and the target is reported at the same
/// place, with the same error, as by a reader that scans the segment from its start, and stays reported by later
/// seeks; a damaged record right before the cursor makes the seek scan from the start (and report it).
#[test]
fn a_continued_seek_reports_damage_like_a_full_scan() {
    for (damaged, continues) in [(30, true), (9, false)] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut writer, 100);
        writer.checkpoint().unwrap();
        drop(writer);
        let mut reader = open_sought(dir.path(), (false, false));
        expect(&mut reader, 0, 10);
        let at = offset(&reader);
        let (path, slot) = locate_record(dir.path(), damaged);
        std::os::unix::fs::FileExt::write_all_at(&open_raw(&path), &[0xEE], slot + 9).unwrap();
        for target in [40, 50] {
            reader.seek(target).unwrap();
            if target == 40 {
                let from = if continues { at } else { FIRST_SLOT };
                assert_eq!(offset(&reader), from, "damage at {damaged}");
            }
            let mut fresh = open(dir.path(), (false, false));
            fresh.seek(target).unwrap();
            let (got, want) = (read_up_to(&mut reader, 5), read_up_to(&mut fresh, 5));
            assert_eq!(got, want);
            assert!(got.1.unwrap().contains(&format!("offset: {slot}")));
            assert_eq!(offset(&reader), slot);
            assert_eq!(reader.position(), target);
        }
    }
}

/// What a seek does when choosing or opening the segment fails does not depend on where it would have started.
#[test]
fn a_forward_seek_that_fails_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 100);
    writer.checkpoint().unwrap();
    writer.close().unwrap();
    let mut reader = open_sought(dir.path(), (false, false));
    expect(&mut reader, 0, 10);
    let path = segments(dir.path())[0].1.clone();
    let raw = open_raw(&path);
    // A damaged header, as at every open of the segment.
    std::os::unix::fs::FileExt::write_all_at(&raw, &[1], 12).unwrap();
    assert!(matches!(
        reader.seek(40),
        Err(Error::Corrupt {
            reason: CorruptReason::BadSegmentHeader(_),
            ..
        })
    ));
    std::os::unix::fs::FileExt::write_all_at(&raw, &[0], 12).unwrap();
    expect(&mut reader, 10, 1);
    // The directory is gone.
    let moved = dir.path().with_extension("moved");
    std::fs::rename(dir.path(), &moved).unwrap();
    assert!(matches!(reader.seek(40), Err(Error::Io { .. })));
    assert!(matches!(reader.seek_to_end(), Err(Error::Io { .. })));
    std::fs::rename(&moved, dir.path()).unwrap();
    expect(&mut reader, 11, 1);
    let at = offset(&reader);
    reader.seek(13).unwrap();
    assert_eq!(offset(&reader), at);
    reader.seek(40).unwrap();
    expect(&mut reader, 40, 60);
}

#[derive(Debug, Clone)]
enum Op {
    Append {
        n: u64,
        len: usize,
    },
    Checkpoint,
    /// Each reader reads this many records.
    Read(usize),
    /// Seeks to this many thousandths of the way from the oldest sequence number to a little past the newest.
    Seek {
        permille: u64,
        read: usize,
    },
    /// Seeks this far ahead of each reader's position.
    Ahead {
        by: u64,
        read: usize,
    },
    /// The records from this many before the end on are lost, and the writer repairs the journal in place.
    RepairInPlace {
        lost: u64,
        len: usize,
    },
    /// The same, by a repair that replaces the segment file.
    RepairByReplacement {
        lost: u64,
        len: usize,
    },
    /// Retention removes the oldest segments, keeping this many.
    Retain(usize),
    ToEnd,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (1u64..60, 0usize..1500).prop_map(|(n, len)| Op::Append { n, len }),
        4 => Just(Op::Checkpoint),
        5 => (1usize..60).prop_map(Op::Read),
        6 => (0u64..1050, 0usize..20).prop_map(|(permille, read)| Op::Seek { permille, read }),
        10 => (0u64..40, 0usize..20).prop_map(|(by, read)| Op::Ahead { by, read }),
        1 => (1u64..20, 0usize..300).prop_map(|(lost, len)| Op::RepairInPlace { lost, len }),
        1 => (1u64..20, 0usize..300).prop_map(|(lost, len)| Op::RepairByReplacement { lost, len }),
        1 => (1usize..4).prop_map(Op::Retain),
        1 => Just(Op::ToEnd),
    ]
}

/// The model: a fresh reader, whose seek always scans the segment from its start. Any sequence of appends, rolls,
/// checkpoints, repairs (in place and by replacement) and removals, with readers of every kind reading and seeking
/// in between: after each seek, a reader returns exactly the records and errors a fresh reader returns after seeking
/// there.
#[test]
fn every_seek_agrees_with_a_fresh_readers() {
    let continued = AtomicU64::new(0);
    let mut runner = TestRunner::new(Config {
        cases: 128,
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(&proptest::collection::vec(op(), 1..60), |ops| {
            run_ops(&ops, &continued);
            Ok(())
        })
        .unwrap();
    // Otherwise the test compared restarts with restarts.
    let continued = continued.into_inner();
    assert!(continued > 500, "only {continued} seeks continued");
}

fn run_ops(ops: &[Op], continued: &AtomicU64) {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 20);
    let mut readers = KINDS.map(|kind| open(dir.path(), kind));
    let mut tag = 100;
    for op in ops {
        match *op {
            Op::Append { n, len } => {
                for i in 0..n {
                    append_tagged(&mut writer, 1, tag, (len * (i as usize % 3)) / 2);
                }
            }
            Op::Checkpoint => writer.checkpoint().unwrap(),
            // Errors are for the next seek to clear (a repair may have left the reader on discarded history).
            Op::Read(n) => {
                for reader in &mut readers {
                    let _ = read_up_to(reader, n);
                }
            }
            Op::Seek { permille, read } => {
                let first = segments(dir.path())[0].0;
                let target = first + (writer.next_seq() - first) * permille / 1000;
                for (reader, kind) in readers.iter_mut().zip(KINDS) {
                    seek_both(dir.path(), reader, kind, target, read, continued);
                }
            }
            Op::Ahead { by, read } => {
                for (reader, kind) in readers.iter_mut().zip(KINDS) {
                    let target = reader.position() + by;
                    seek_both(dir.path(), reader, kind, target, read, continued);
                }
            }
            Op::RepairInPlace { lost, len } | Op::RepairByReplacement { lost, len } => {
                // Records that may be lost: in the newest segment, not covered by the checkpoint.
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
                if matches!(op, Op::RepairInPlace { .. }) {
                    zero_commit_words_from(dir.path(), at);
                } else {
                    corrupt_by_replacement(dir.path(), at);
                }
                writer = Writer::open(dir.path(), small_config()).unwrap();
                assert_eq!(writer.next_seq(), at);
                tag += 1;
                append_tagged(&mut writer, lost + 3, tag, len);
            }
            Op::Retain(keep) => {
                let policy = Policy::default().max_segments(keep);
                let plan = retention::plan(dir.path(), &policy).unwrap();
                retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
            }
            Op::ToEnd => {
                for (reader, kind) in readers.iter_mut().zip(KINDS) {
                    reader.seek_to_end().unwrap();
                    let mut fresh = open(dir.path(), kind);
                    let (_, end) = read_up_to(&mut fresh, usize::MAX);
                    assert_eq!(end.as_deref(), Some("end"));
                    assert_eq!(reader.position(), fresh.position(), "{kind:?}");
                    assert!(reader.try_next().unwrap().is_none());
                }
            }
        }
    }
}

fn seek_both(
    dir: &Path,
    reader: &mut Reader,
    kind: (bool, bool),
    target: u64,
    read: usize,
    continued: &AtomicU64,
) {
    let mut fresh = open(dir, kind);
    let (got, want) = (reader.seek(target), fresh.seek(target));
    assert_eq!(format!("{got:?}"), format!("{want:?}"), "{kind:?}");
    if got.is_err() {
        return;
    }
    assert_eq!(offset(&fresh), FIRST_SLOT, "the model must scan");
    if offset(reader) > FIRST_SLOT {
        continued.fetch_add(1, Ordering::Relaxed);
    }
    assert_eq!(reader.position(), target);
    let got = read_up_to(reader, read);
    let want = read_up_to(&mut fresh, read);
    assert_eq!(got, want, "{kind:?}: seek to {target}");
    assert_eq!(reader.position(), fresh.position());
    if read > 0 {
        assert_eq!(offset(reader), offset(&fresh));
    }
}

#[test]
fn the_end_is_found_without_reading_older_segments() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 2_000);
    let segs = segments(dir.path());
    assert!(segs.len() > 3);
    // Damage in an older segment, and a whole one missing.
    let (path, slot) = locate_record(dir.path(), segs[1].0 + 3);
    std::os::unix::fs::FileExt::write_all_at(&open_raw(&path), &[0xEE; 8], slot + 9).unwrap();
    std::fs::remove_file(&segs[2].1).unwrap();
    for mapped in [false, true] {
        let mut reader = open(dir.path(), (mapped, false));
        reader.seek_to_end().unwrap();
        assert_eq!(reader.position(), 2_000);
        assert!(reader.try_next().unwrap().is_none());
    }
    // In the newest segment, the scan starts at the marker: damage before it is not read either, damage after it is
    // reported, and the reader then stays where it was.
    writer.checkpoint().unwrap();
    append_n(&mut writer, 10);
    assert_eq!(segments(dir.path()).last(), segs.last(), "no roll");
    let (path, slot) = locate_record(dir.path(), 1_998);
    std::os::unix::fs::FileExt::write_all_at(&open_raw(&path), &[0xEE; 8], slot + 9).unwrap();
    for mapped in [false, true] {
        let mut reader = open(dir.path(), (mapped, false));
        reader.seek_to_end().unwrap();
        assert_eq!(reader.position(), 2_010);
        let mut durable = open(dir.path(), (mapped, true));
        durable.seek_to_end().unwrap();
        assert_eq!(durable.position(), 2_000);
        assert!(durable.try_next().unwrap().is_none());
    }
    let (path, _) = locate_record(dir.path(), 1_997);
    let after = {
        let mut reader = open(dir.path(), (false, false));
        reader.seek_to_end().unwrap();
        offset(&reader)
    };
    // The slot after the last record: not a record, not empty.
    std::os::unix::fs::FileExt::write_all_at(&open_raw(&path), &[0xEE; 8], after).unwrap();
    for mapped in [false, true] {
        let mut reader = open(dir.path(), (mapped, false));
        expect(&mut reader, 0, 2);
        match reader.seek_to_end() {
            Err(Error::Corrupt { offset, .. }) => assert_eq!(offset, after),
            other => panic!("{other:?}"),
        }
        assert_eq!(reader.position(), 2);
        expect(&mut reader, 2, 2);
    }
}

#[test]
fn the_end_of_empty_unchecked_and_just_rolled_journals() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    for kind in KINDS {
        let mut reader = open(dir.path(), kind);
        reader.seek_to_end().unwrap();
        assert_eq!(reader.position(), 0, "{kind:?}");
    }
    append_n(&mut writer, 500);
    let first = segments(dir.path())[0].0;
    for kind in KINDS {
        let mut reader = open(dir.path(), kind);
        reader.seek_to_end().unwrap();
        // Nothing is covered without a checkpoint.
        assert_eq!(reader.position(), if kind.1 { first } else { 500 });
        assert!(reader.try_next().unwrap().is_none());
    }
    // A roll whose new segment is still empty: a fill that panics after the roll leaves that.
    writer.checkpoint().unwrap();
    let covered = writer.next_seq();
    let big = vec![0u8; 60_000];
    let mut next = 500;
    let record = NewRecord {
        schema_id: 1,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload: &big,
    };
    while segments(dir.path()).last().unwrap().0 != next {
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = writer.append_with(&record, big.len(), |_| panic!("fill failed"));
        }));
        assert!(panicked.is_err());
        next = writer.next_seq();
    }
    for kind in KINDS {
        let mut reader = open(dir.path(), kind);
        reader.seek_to_end().unwrap();
        assert_eq!(reader.position(), covered, "{kind:?}");
        assert!(reader.try_next().unwrap().is_none());
    }
    append_n(&mut writer, 3);
    writer.checkpoint().unwrap();
    for (kind, end) in KINDS.into_iter().zip(covered + 3..) {
        let mut reader = open(dir.path(), kind);
        reader.seek(covered).unwrap();
        expect(&mut reader, covered, 3);
        reader.seek_to_end().unwrap();
        assert_eq!(reader.position(), end, "{kind:?}");
        append_n(&mut writer, 1);
        writer.checkpoint().unwrap();
        expect(&mut reader, end, 1);
        // The reader can go back, and a seek from the end that passed no record yet is not taken for a continued one.
        reader.seek_to_end().unwrap();
        reader.seek(covered).unwrap();
        expect(&mut reader, covered, 1);
    }
}

/// A durable-only reader's end is the checkpoint, also when the writer has rolled past it.
#[test]
fn a_durable_only_readers_end_is_the_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 300);
    writer.checkpoint().unwrap();
    append_n(&mut writer, 700);
    for mapped in [false, true] {
        let mut reader = open(dir.path(), (mapped, true));
        reader.seek_to_end().unwrap();
        assert_eq!(reader.position(), 300);
        assert!(offset(&reader) > FIRST_SLOT, "placed at the marker");
        assert!(reader.try_next().unwrap().is_none());
    }
    writer.checkpoint().unwrap();
    let mut reader = open(dir.path(), (false, true));
    reader.seek_to_end().unwrap();
    assert_eq!(reader.position(), 1_000);
    // The marker moves while the call runs: the end moves with it.
    append_n(&mut writer, 5);
    let mut reader = open(dir.path(), (false, true));
    tickertape::fault::clear();
    let moved = Rc::new(Cell::new(false));
    let m = moved.clone();
    tickertape::fault::run_at(Point::ReaderCheckpoint, 0, move || {
        writer.checkpoint().unwrap();
        m.set(true);
    });
    reader.seek_to_end().unwrap();
    assert!(moved.get());
    assert_eq!(reader.position(), 1_005);
}

/// Recovery replaces the segment while a durable-only reader looks for the end in it: the reader finds out (ADR 0034)
/// and starts over in the new file. If that happens every time, it reports `Discarded` and stays where it was.
#[test]
fn the_end_is_found_again_when_recovery_replaces_the_segment() {
    fn replace(dir: &Path) {
        let path = segments(dir).pop().unwrap().1;
        let copy = dir.join("copy");
        std::fs::copy(&path, &copy).unwrap();
        std::fs::rename(&copy, &path).unwrap();
    }
    for times in [1u32, 7, 8] {
        for mapped in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut writer = Writer::open(dir.path(), small_config()).unwrap();
            append_n(&mut writer, 20);
            writer.checkpoint().unwrap();
            drop(writer);
            let mut reader = open(dir.path(), (mapped, true));
            expect(&mut reader, 0, 5);
            fn arm(dir: std::path::PathBuf, left: u32) {
                // After the marker is read and the segment opened, before the reader is done with it.
                tickertape::fault::run_at(Point::ReaderCheckpoint, 0, move || {
                    replace(&dir);
                    if left > 1 {
                        arm(dir.clone(), left - 1);
                    }
                });
            }
            tickertape::fault::clear();
            arm(dir.path().to_path_buf(), times);
            let result = reader.seek_to_end();
            tickertape::fault::clear();
            if times < 8 {
                result.unwrap();
                assert_eq!(reader.position(), 20);
                assert!(reader.try_next().unwrap().is_none());
            } else {
                assert!(matches!(result, Err(Error::Discarded { .. })), "{result:?}");
                assert_eq!(reader.position(), 5);
                reader.seek_to_end().unwrap();
                assert_eq!(reader.position(), 20);
            }
        }
    }
}

/// A retention run removes every older segment, and the writer rolls, between the listing and the open.
#[test]
fn the_end_under_retention_and_rolls() {
    for kind in KINDS {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut writer, 600);
        writer.checkpoint().unwrap();
        let mut reader = open(dir.path(), kind);
        tickertape::fault::clear();
        let path = dir.path().to_path_buf();
        tickertape::fault::run_at(Point::ReaderOpen, 0, move || {
            append_n(&mut writer, 600);
            writer.checkpoint().unwrap();
            let plan = retention::plan(&path, &Policy::default().max_segments(1)).unwrap();
            retention::apply(&path, &plan, &Action::Delete).unwrap();
        });
        reader.seek_to_end().unwrap();
        assert_eq!(segments(dir.path()).len(), 1);
        assert_eq!(reader.position(), 1_200, "{kind:?}");
        assert!(reader.try_next().unwrap().is_none());
    }
}

/// Under a live writer, everything committed before the call is behind the reader, nothing committed after it is, and
/// from there the reader returns every record in order.
#[test]
fn the_end_under_a_live_writer() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 50);
    let started = AtomicU64::new(50); // appends that may have begun
    let committed = AtomicU64::new(50);
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(Ordering::SeqCst) {
                started.fetch_add(1, Ordering::SeqCst);
                append_n(&mut writer, 1);
                committed.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_micros(20));
            }
        });
        for round in 0..200 {
            let mut reader = open(dir.path(), (round % 2 == 1, false));
            let before = committed.load(Ordering::SeqCst);
            reader.seek_to_end().unwrap();
            let after = started.load(Ordering::SeqCst);
            let at = reader.position();
            assert!(
                (before..=after).contains(&at),
                "{before} <= {at} <= {after}"
            );
            let record = reader
                .next_timeout(Some(Duration::from_secs(10)))
                .unwrap()
                .unwrap()
                .to_owned();
            assert_eq!(record.seq, at);
            check_record(&record);
        }
        stop.store(true, Ordering::SeqCst);
    });
}

/// A durable-only reader that had applied a marker which is gone: nothing is covered now, so its end is the first
/// record of the oldest segment, as for a new reader, not where the old marker was.
#[test]
fn a_durable_only_readers_end_without_a_marker_is_the_oldest_record() {
    for mapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut writer, 2_000);
        writer.checkpoint().unwrap();
        let mut reader = open(dir.path(), (mapped, true));
        expect(&mut reader, 0, 5);
        std::fs::remove_file(dir.path().join("checkpoint")).unwrap();
        reader.seek_to_end().unwrap();
        assert_eq!(reader.position(), 0);
        assert!(reader.try_next().unwrap().is_none());
        // The marker can't be read: an error, and the reader stays where it was.
        std::fs::create_dir(dir.path().join("checkpoint")).unwrap();
        assert!(matches!(reader.seek_to_end(), Err(Error::Io { .. })));
        let mut visible = open(dir.path(), (mapped, false));
        visible.seek_to_end().unwrap();
        assert_eq!(visible.position(), 2_000);
    }
}

/// A lower marker than the one the reader had applied: the end is where the marker is now.
#[test]
fn a_durable_only_readers_end_is_the_marker_as_it_is_now() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 100);
    writer.checkpoint().unwrap();
    let old = std::fs::read(dir.path().join("checkpoint")).unwrap();
    append_n(&mut writer, 50);
    writer.checkpoint().unwrap();
    let mut reader = open(dir.path(), (false, true));
    expect(&mut reader, 0, 5);
    std::fs::write(dir.path().join("checkpoint"), old).unwrap();
    reader.seek_to_end().unwrap();
    assert_eq!(reader.position(), 100);
    assert!(reader.try_next().unwrap().is_none());
}

/// The marker is missing when the call reads it and back right after: the call must not read the journal from its
/// oldest segment (which is damaged here, so it would fail).
#[test]
fn a_marker_that_reappears_does_not_make_seek_to_end_read_old_segments() {
    for mapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut writer, 2_000);
        writer.checkpoint().unwrap();
        let mut reader = open(dir.path(), (mapped, true));
        expect(&mut reader, 0, 5);
        let (path, slot) = locate_record(dir.path(), segments(dir.path())[1].0 + 3);
        std::os::unix::fs::FileExt::write_all_at(&open_raw(&path), &[0xEE; 8], slot + 9).unwrap();
        let (marker, hidden) = (dir.path().join("checkpoint"), dir.path().join("hidden"));
        std::fs::rename(&marker, &hidden).unwrap();
        tickertape::fault::clear();
        tickertape::fault::run_at(Point::ReaderOpen, 0, move || {
            std::fs::rename(&hidden, &marker).unwrap();
        });
        reader.seek_to_end().unwrap();
        assert_eq!(reader.position(), 0);
        reader.seek_to_end().unwrap();
        assert_eq!(reader.position(), 2_000);
    }
}

/// Crash contract C13: a failed `seek_to_end` that had applied a newer marker to the current file forgets it again, so
/// that a durable-only reader in a replaced file does not take the removed records there for covered ones.
#[test]
fn a_failed_seek_to_end_restores_the_durable_boundary() {
    for mapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut writer, 10);
        writer.checkpoint().unwrap();
        append_n(&mut writer, 10);
        drop(writer);
        let mut reader = open(dir.path(), (mapped, true));
        expect(&mut reader, 0, 10);
        assert!(reader.next_owned().unwrap().is_none());
        corrupt_by_replacement(dir.path(), 13);
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut writer, 20);
        writer.checkpoint().unwrap();
        tickertape::fault::clear();
        let path = dir.path().to_path_buf();
        tickertape::fault::run_at(Point::ReaderCheckpoint, 0, move || {
            let first = writer.next_seq();
            append_n(&mut writer, 10);
            writer.checkpoint().unwrap();
            let (p, slot) = locate_record(&path, first + 2);
            std::os::unix::fs::FileExt::write_all_at(&open_raw(&p), &[0xEE; 8], slot + 9).unwrap();
        });
        assert!(matches!(reader.seek_to_end(), Err(Error::Corrupt { .. })));
        assert_eq!(reader.position(), 10);
        match reader.next_owned() {
            Err(Error::Discarded { seq: 10, .. }) => {}
            other => panic!("mapped {mapped}: {other:?}"),
        }
    }
}

/// A failed `seek_to_end` leaves the reader exactly as it was, its read buffer included: it goes on returning the same
/// records as a reader that did not make the call.
#[test]
fn a_failed_seek_to_end_keeps_the_read_buffer_and_the_seek_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 30);
    writer.checkpoint().unwrap();
    drop(writer);
    let mut readers = [0, 1].map(|_| {
        let mut reader = open_sought(dir.path(), (false, false));
        expect(&mut reader, 0, 5);
        reader
    });
    // Damage the buffered records 20 to 29 in the file, and what `seek_to_end` reads first.
    let end = {
        let mut reader = open(dir.path(), (false, false));
        reader.seek_to_end().unwrap();
        offset(&reader)
    };
    zero_commit_words_from(dir.path(), 20);
    let path = segments(dir.path())[0].1.clone();
    std::os::unix::fs::FileExt::write_all_at(&open_raw(&path), &[0xEE; 4], end).unwrap();
    assert!(matches!(
        readers[1].seek_to_end(),
        Err(Error::Corrupt { .. })
    ));
    let at = offset(&readers[1]);
    let [a, b] = readers.each_mut().map(|r| read_up_to(r, 100));
    assert_eq!(a, b);
    assert_eq!(a.0.len(), 25);
    // The coverage too: a seek continues from where the reader was.
    let mut reader = open_sought(dir.path(), (false, false));
    expect(&mut reader, 0, 5);
    assert!(reader.seek_to_end().is_err());
    reader.seek(10).unwrap();
    assert_eq!(offset(&reader), at);
}

/// A failed `seek_to_end` that had read a newer marker does not leave its coverage behind: the reader passed its
/// records before that marker.
#[test]
fn a_failed_seek_to_end_restores_the_seek_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 10);
    writer.checkpoint().unwrap();
    append_n(&mut writer, 20);
    let mut reader = open_sought(dir.path(), (false, false));
    expect(&mut reader, 0, 18);
    writer.checkpoint().unwrap();
    append_n(&mut writer, 3);
    let (path, slot) = locate_record(dir.path(), 31);
    std::os::unix::fs::FileExt::write_all_at(&open_raw(&path), &[0xEE; 8], slot + 9).unwrap();
    assert!(matches!(reader.seek_to_end(), Err(Error::Corrupt { .. })));
    reader.seek(20).unwrap();
    assert_eq!(offset(&reader), FIRST_SLOT);
    expect(&mut reader, 20, 1);
}
