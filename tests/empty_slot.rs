//! An empty slot that cannot be the head of the journal is reported as corruption (ADR 0050, issue #79), and no other
//! empty slot is: every legitimate one is a test here, and a live reader of every kind must never see a false error.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use tickertape::fault::{self, Point};
use tickertape::retention::{self, Action, Policy};
use tickertape::{
    CorruptReason, Error, NewRecord, OwnedRecord, Reader, ReaderConfig, Writer, format, inspect,
};
use tickertape_tests::{
    Rng, SMALL_SEGMENT, append_n, check_record, corrupt_by_replacement, create_raw_segment,
    end_of_records, kill_len, locate_record, open_raw, payload, segments, small_config,
    zero_commit_words_from,
};

/// The kinds of reader: (mapped, durable-only).
const KINDS: [(bool, bool); 4] = [(false, false), (true, false), (false, true), (true, true)];
const VISIBLE: [(bool, bool); 2] = [(false, false), (true, false)];

/// A reader checks a slot it waits at after this long (and the segments at most every second).
const CHECK: Duration = Duration::from_millis(100);

fn open(dir: &Path, (mapped, durable): (bool, bool)) -> Reader {
    let config = ReaderConfig::default().durable_only(durable).notify(false);
    if mapped {
        // SAFETY: besides tickertape writers, only these tests' injected damage modifies the segment files, and no
        // reader holds a payload meanwhile.
        unsafe { Reader::open_mapped(dir, config) }.unwrap()
    } else {
        Reader::open(dir, config).unwrap()
    }
}

/// Every record from the cursor on, and how reading ended.
fn drain(reader: &mut Reader) -> (Vec<OwnedRecord>, Result<(), Error>) {
    let mut records = Vec::new();
    loop {
        match reader.next_owned() {
            Ok(Some(record)) => records.push(record),
            Ok(None) => return (records, Ok(())),
            Err(e) => return (records, Err(e)),
        }
    }
}

/// Whether `result` is the report of an empty slot at `at` in `path`.
fn is_report(result: &Result<(), Error>, path: &Path, at: u64) -> bool {
    matches!(
        result,
        Err(Error::Corrupt { path: p, offset, reason: CorruptReason::EmptyCommitWord(_), .. })
            if p == path && *offset == at
    )
}

/// Reads to the end and expects the report of the slot of `seq` there, by every way to read, again and again.
fn expect_report(reader: &mut Reader, path: &Path, at: u64, seq: u64, case: &str) {
    let (_, end) = drain(reader);
    assert!(is_report(&end, path, at), "{case}: {end:?}");
    assert_eq!(reader.position(), seq, "{case}");
    let again = reader.try_next().map(|r| assert!(r.is_none(), "{case}"));
    assert!(is_report(&again, path, at), "{case}: {again:?}");
    let waited = reader
        .next_timeout(Some(Duration::from_secs(5)))
        .map(|r| assert!(r.is_none(), "{case}"));
    assert!(is_report(&waited, path, at), "{case}: {waited:?}");
    let mut records = reader.records();
    let item = records.next().expect(case).map(|_| ());
    assert!(is_report(&item, path, at), "{case}: {item:?}");
    assert!(records.next().is_none(), "{case}");
    assert_eq!(reader.position(), seq, "{case}");
}

/// Nothing is reported at the cursor: not at the first check, and not by those a waiting reader makes (two of which list
/// the segments).
fn expect_wait(readers: &mut [Reader], case: &str) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1_300) {
        for (i, reader) in readers.iter_mut().enumerate() {
            match reader.try_next() {
                Ok(None) => {}
                other => panic!("{case}, reader {i}: {other:?}"),
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Readers of every kind at the head: read to the end now, and opened now and read to the end.
fn at_the_end(dir: &Path, kinds: &[(bool, bool)]) -> Vec<Reader> {
    kinds
        .iter()
        .map(|&kind| {
            let mut reader = open(dir, kind);
            let (records, end) = drain(&mut reader);
            assert!(end.is_ok(), "{kind:?}: {end:?}");
            records.iter().for_each(check_record);
            reader
        })
        .collect()
}

fn zero(path: &Path, at: u64, len: usize) {
    open_raw(path).write_all_at(&vec![0; len], at).unwrap();
}

/// A journal of at least `count` segments, and its writer.
fn journal(dir: &Path, count: usize) -> Writer {
    let mut writer = Writer::open(dir, small_config()).unwrap();
    append_n(&mut writer, 50);
    while segments(dir).len() < count {
        append_n(&mut writer, 50);
    }
    writer
}

/// The issue's reproduction: one zeroed commit word in the middle of a journal, the marker in the newest segment.
#[test]
fn a_zeroed_word_in_older_history_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = journal(dir.path(), 16);
    writer.checkpoint().unwrap();
    let segments = segments(dir.path());
    let (base, path) = &segments[8];
    for seq in [*base, base + 30, segments[9].0 - 1] {
        let (found, at) = locate_record(dir.path(), seq);
        assert_eq!(&found, path);
        let mut word = [0; 4];
        open_raw(path).read_exact_at(&mut word, at).unwrap();
        zero(path, at, 4);
        for kind in KINDS {
            let case = format!("{kind:?}, record {seq}");
            let mut reader = open(dir.path(), kind);
            let (records, end) = drain(&mut reader);
            assert_eq!(records.len() as u64, seq, "{case}");
            assert!(is_report(&end, path, at), "{case}: {end:?}");
            expect_report(&mut reader, path, at, seq, &case);
            // By a seek too, and not what comes before it or after it.
            let mut reader = open(dir.path(), kind);
            reader.seek(seq).unwrap();
            expect_report(&mut reader, path, at, seq, &case);
            reader.seek(segments[9].0).unwrap();
            let (records, end) = drain(&mut reader);
            assert!(end.is_ok(), "{case}: {end:?}");
            assert_eq!(records.last().unwrap().seq, writer.next_seq() - 1);
            // `seek_to_end` does not read there.
            reader.seek_to_end().unwrap();
            assert!(reader.try_next().unwrap().is_none());
        }
        // `verify` reports the same place.
        let report = inspect::verify(dir.path()).unwrap();
        assert!(
            report
                .problems
                .iter()
                .any(|p| &p.path == path && p.offset == at),
            "{:?}",
            report.problems
        );
        // Repaired from a copy, the same readers go on.
        let mut reader = open(dir.path(), (false, false));
        assert!(drain(&mut reader).1.is_err());
        open_raw(path).write_all_at(&word, at).unwrap();
        let (records, end) = drain(&mut reader);
        assert!(end.is_ok(), "{end:?}");
        assert_eq!(records[0].seq, seq);
    }
}

/// Each kind of evidence on its own: what the marker says, and what a later segment says with no marker at all.
#[test]
fn each_kind_of_evidence_reports_the_slot() {
    let reported =
        |dir: &Path, kinds: &[(bool, bool)], path: &Path, at: u64, seq: u64, case: &str| {
            for &kind in kinds {
                expect_report(
                    &mut open(dir, kind),
                    path,
                    at,
                    seq,
                    &format!("{case}, {kind:?}"),
                );
            }
            let problems = inspect::verify(dir).unwrap().problems;
            assert!(
                problems.iter().any(|p| p.path == path && p.offset == at),
                "{case}: {problems:?}"
            );
        };
    // The marker, in the only segment: a record in front of it (so that nothing after it shows that it is missing too).
    for tail in [4, 400] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = journal(dir.path(), 1);
        writer.checkpoint().unwrap();
        let last = writer.next_seq() - 1;
        let (path, at) = locate_record(dir.path(), last);
        zero(&path, at, tail);
        reported(dir.path(), &KINDS, &path, at, last, "marker");
    }
    // The marker at the first slot of an empty segment, and the seal in front of it.
    let dir = tempfile::tempdir().unwrap();
    let mut writer = journal(dir.path(), 2);
    let next = writer.next_seq();
    let declined = writer.try_append_with(
        &NewRecord {
            schema_id: 0,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: &[],
        },
        SMALL_SEGMENT as usize - 112,
        |_| Err(()),
    );
    assert!(matches!(declined, Ok(Err(()))));
    assert_eq!(segments(dir.path()).last().unwrap().0, next);
    writer.checkpoint().unwrap();
    let path = segments(dir.path()).iter().rev().nth(1).unwrap().1.clone();
    let (at, end) = end_of_records(&path);
    assert_eq!(end, next);
    let mut waiting = at_the_end(dir.path(), &KINDS);
    zero(&path, at, 8);
    reported(
        dir.path(),
        &KINDS,
        &path,
        at,
        next,
        "seal before the marker",
    );
    // Those that had followed the seal are not affected.
    expect_wait(&mut waiting, "behind the seal");
    drop(writer);

    // No marker from here on. A record, and a seal, with segments after the next one.
    let dir = tempfile::tempdir().unwrap();
    drop(journal(dir.path(), 4));
    std::fs::remove_file(dir.path().join("checkpoint")).unwrap();
    let listed = segments(dir.path());
    let (path, at) = locate_record(dir.path(), listed[1].0 + 5);
    let image = std::fs::read(&path).unwrap();
    zero(&path, at, 4);
    let case = "record, later segments";
    reported(dir.path(), &VISIBLE, &path, at, listed[1].0 + 5, case);
    open_raw(&path).write_all_at(&image, 0).unwrap();
    let (at, end) = end_of_records(&path);
    assert_eq!(end, listed[2].0);
    zero(&path, at, 8);
    reported(dir.path(), &VISIBLE, &path, at, end, "seal, later segments");
    open_raw(&path).write_all_at(&image, 0).unwrap();
    // The last seal: the newest segment, which holds a record, is the only one after it.
    let path = listed[2].1.clone();
    let (at, end) = end_of_records(&path);
    assert_eq!(end, listed[3].0);
    zero(&path, at, 8);
    reported(dir.path(), &VISIBLE, &path, at, end, "last seal");
    // Durable-only readers have nothing to read there.
    for kind in [(false, true), (true, true)] {
        assert!(open(dir.path(), kind).try_next().unwrap().is_none());
    }
}

/// A durable-only reader waiting at the marker, at the end of a segment whose seal is damaged while the marker names the
/// next one: the marker does not move past it, so it finds out where it waits.
#[test]
fn a_durable_only_reader_waiting_at_a_missing_seal_reports_it() {
    for mapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = journal(dir.path(), 1);
        writer.checkpoint().unwrap();
        let mut reader = open(dir.path(), (mapped, true));
        // Exactly one segment of records, then a roll without a record.
        loop {
            append_n(&mut writer, 1);
            if segments(dir.path()).len() == 2 {
                break;
            }
        }
        let base = segments(dir.path())[1].0;
        let path = segments(dir.path())[0].1.clone();
        writer.checkpoint().unwrap();
        // The marker names the second segment, after its first record. Put it at its first slot.
        let slot = format::encode_checkpoint_slot(&format::CheckpointSlot {
            generation: 1_000,
            next_seq: base,
            segment_base_seq: base,
            offset: 64,
        });
        std::fs::write(dir.path().join("checkpoint"), slot).unwrap();
        let (at, _) = end_of_records(&path);
        zero(&path, at, 8);
        let (records, end) = drain(&mut reader);
        assert_eq!(records.len() as u64, base);
        assert!(is_report(&end, &path, at), "{end:?}");
        expect_report(&mut reader, &path, at, base, "waiting");
    }
}

/// Damage that nothing above covers: the head, as far as anyone can tell. Stated in the contract.
#[test]
fn what_nothing_covers_is_not_reported() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = journal(dir.path(), 2);
    writer.checkpoint().unwrap();
    append_n(&mut writer, 10);
    let seq = writer.next_seq() - 4;
    let (path, at) = locate_record(dir.path(), seq);
    let mut word = [0; 4];
    open_raw(&path).read_exact_at(&mut word, at).unwrap();
    zero(&path, at, 4);
    let mut readers: Vec<_> = KINDS.map(|kind| open(dir.path(), kind)).into();
    for (reader, kind) in readers.iter_mut().zip(KINDS) {
        let (records, end) = drain(reader);
        assert!(end.is_ok(), "{end:?}");
        assert_eq!(records.len() as u64, if kind.1 { seq - 6 } else { seq });
    }
    expect_wait(&mut readers, "above the marker");
    // Until a checkpoint covers it (the writer holds the records), or the writer rolls.
    writer.checkpoint().unwrap();
    for reader in &mut readers {
        reader.seek(reader.position()).unwrap();
        expect_report(reader, &path, at, seq, "covered");
    }
    open_raw(&path).write_all_at(&word, at).unwrap();
    for reader in &mut readers {
        let (records, end) = drain(reader);
        assert!(end.is_ok() && records[0].seq == seq);
    }
    // The last segment of a journal without a marker; and an older marker.
    let marker = dir.path().join("checkpoint");
    let full = std::fs::read(&marker).unwrap();
    for how in ["missing", "unreadable", "invalid", "set back"] {
        let _ = std::fs::remove_dir(&marker);
        let _ = std::fs::remove_file(&marker);
        match how {
            "unreadable" => std::fs::create_dir(&marker).unwrap(),
            "invalid" => std::fs::write(&marker, [7; 131_072]).unwrap(),
            "set back" => {
                let mut older = full.clone();
                let newer = format::CHECKPOINT_SLOT_OFFSETS
                    .into_iter()
                    .max_by_key(|&at| {
                        u64::from_le_bytes(full[at + 16..at + 24].try_into().unwrap())
                    })
                    .unwrap();
                older[newer..newer + 64].fill(0);
                std::fs::write(&marker, older).unwrap();
            }
            _ => {}
        }
        zero(&path, at, 4);
        let mut readers: Vec<_> = VISIBLE.map(|kind| open(dir.path(), kind)).into();
        for reader in &mut readers {
            let (records, end) = drain(reader);
            assert!(end.is_ok(), "{how}: {end:?}");
            assert_eq!(records.len() as u64, seq);
        }
        expect_wait(&mut readers, how);
        open_raw(&path).write_all_at(&word, at).unwrap();
    }
}

/// The window of a roll: the successor is linked, and the seal not stored. Readers of every kind wait there, however long
/// it takes, before and after the marker moves.
#[test]
fn a_roll_in_progress_is_not_reported() {
    for checkpointed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = journal(dir.path(), 2);
        if checkpointed {
            writer.checkpoint().unwrap();
        }
        fault::clear();
        let path = dir.path().to_path_buf();
        let seen = Rc::new(Cell::new(false));
        let reached = seen.clone();
        let count = segments(dir.path()).len();
        let mut early = at_the_end(dir.path(), &KINDS);
        fault::run_at(Point::RollAfterLink, 0, move || {
            assert_eq!(segments(&path).len(), count + 1);
            for reader in &mut early {
                let (records, end) = drain(reader);
                assert!(end.is_ok(), "{end:?}");
                records.iter().for_each(check_record);
            }
            expect_wait(&mut early, "roll, reader opened before");
            expect_wait(&mut at_the_end(&path, &KINDS), "roll, reader opened in it");
            let mut ended = open(&path, (false, false));
            ended.seek_to_end().unwrap();
            assert!(ended.try_next().unwrap().is_none());
            reached.set(true);
        });
        while !seen.get() {
            append_n(&mut writer, 1);
        }
        writer.checkpoint().unwrap();
        for reader in &mut at_the_end(dir.path(), &KINDS) {
            assert_eq!(reader.position(), writer.next_seq());
        }
    }
}

/// The writer died in that window; its successor, empty, and the next recovery, which seals.
#[test]
fn a_crash_between_link_and_seal_is_not_reported() {
    for torn in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = journal(dir.path(), 2);
        writer.checkpoint().unwrap();
        append_n(&mut writer, 5);
        let next = writer.next_seq();
        drop(writer);
        let successor = create_raw_segment(dir.path(), next, SMALL_SEGMENT);
        if torn {
            // A body without its commit word, in the successor.
            open_raw(&successor).write_all_at(&[9; 80], 68).unwrap();
        }
        let mut readers = at_the_end(dir.path(), &KINDS);
        expect_wait(&mut readers, "crash in a roll");
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        expect_wait(&mut readers, "recovered");
        append_n(&mut writer, 3);
        writer.checkpoint().unwrap();
        for reader in &mut readers {
            let (records, end) = drain(reader);
            assert!(end.is_ok(), "{end:?}");
            records.iter().for_each(check_record);
            assert_eq!(reader.position(), next + 3);
        }
    }
}

/// A torn append: before recovery, after it zeroed the bytes, and when the writer went on, checkpointed and rolled. And a
/// newest segment that recovery created again, empty.
#[test]
fn a_torn_append_and_a_recreated_segment_are_not_reported() {
    for checkpointed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = journal(dir.path(), 2);
        if checkpointed {
            writer.checkpoint().unwrap();
        }
        let next = writer.next_seq();
        drop(writer);
        let path = segments(dir.path()).last().unwrap().1.clone();
        let (at, _) = end_of_records(&path);
        open_raw(&path).write_all_at(&[5; 200], at + 4).unwrap();
        let mut readers = at_the_end(dir.path(), &VISIBLE);
        expect_wait(&mut readers, "torn");
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        expect_wait(&mut readers, "zeroed");
        // The new record is shorter than the torn one; then everything that could count as evidence, before a reader looks.
        append_n(&mut writer, 1);
        writer.checkpoint().unwrap();
        let count = segments(dir.path()).len();
        while segments(dir.path()).len() < count + 3 {
            append_n(&mut writer, 20);
        }
        writer.checkpoint().unwrap();
        for reader in &mut readers {
            let (records, end) = drain(reader);
            assert!(end.is_ok(), "{end:?}");
            records.iter().for_each(check_record);
            assert_eq!(records[0].seq, next);
            assert_eq!(reader.position(), writer.next_seq());
        }
        // The newest segment is gone (it was never synced); the one before it is sealed. Recovery creates it again.
        if !checkpointed {
            continue;
        }
        drop(writer);
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        loop {
            let count = segments(dir.path()).len();
            append_n(&mut writer, 1);
            if segments(dir.path()).len() > count {
                break;
            }
        }
        drop(writer);
        let (base, path) = segments(dir.path()).pop().unwrap();
        std::fs::remove_file(path).unwrap();
        let _writer = Writer::open(dir.path(), small_config()).unwrap();
        assert_eq!(segments(dir.path()).last().unwrap().0, base);
        expect_wait(&mut at_the_end(dir.path(), &KINDS), "recreated");
    }
}

/// The record, the roll, the checkpoint and more segments come between a reader's read of the slot and its look for
/// evidence, at each point of the check: the slot is read again after the evidence.
#[test]
fn a_slot_that_fills_during_the_check_is_not_reported() {
    for point in [
        Point::ReaderEmptySlotCheck,
        Point::ReaderEmptySlot,
        Point::ReaderEmptySlotConfirm,
    ] {
        for kind in KINDS {
            for records in [1, 600] {
                let case = format!("{point:?}, {kind:?}, {records} records");
                let dir = tempfile::tempdir().unwrap();
                let mut writer = journal(dir.path(), 2);
                writer.checkpoint().unwrap();
                let next = writer.next_seq();
                let damaged = (point != Point::ReaderEmptySlotCheck).then(|| {
                    // Reaching these points takes evidence: a covered slot that is empty, and is filled at the point.
                    append_n(&mut writer, 1);
                    writer.checkpoint().unwrap();
                    let (path, at) = locate_record(dir.path(), next);
                    let mut word = [0; 4];
                    open_raw(&path).read_exact_at(&mut word, at).unwrap();
                    zero(&path, at, 4);
                    (path, at, word)
                });
                let mut reader = open(dir.path(), kind);
                fault::clear();
                let fired = Rc::new(Cell::new(false));
                let fire = fired.clone();
                let shared = Rc::new(std::cell::RefCell::new(writer));
                let w = shared.clone();
                fault::run_at(point, 0, move || {
                    fire.set(true);
                    if let Some((path, at, word)) = damaged {
                        open_raw(&path).write_all_at(&word, at).unwrap();
                    }
                    append_n(&mut w.borrow_mut(), records);
                    w.borrow_mut().checkpoint().unwrap();
                });
                let (first, end) = drain(&mut reader);
                assert!(end.is_ok(), "{case}: {end:?}");
                // A durable-only reader does not look at a slot that is not covered.
                assert!(fired.get() || kind.1, "{case}");
                fault::clear();
                let (rest, end) = drain(&mut reader);
                assert!(end.is_ok(), "{case}: {end:?}");
                first.iter().chain(&rest).for_each(check_record);
                assert_eq!(reader.position(), shared.borrow().next_seq(), "{case}");
            }
        }
    }
}

/// The evidence goes away while the slot is read again (a recovery moves a disconnected segment into quarantine).
#[test]
fn evidence_that_goes_away_is_not_reported() {
    for kind in VISIBLE {
        let dir = tempfile::tempdir().unwrap();
        let writer = journal(dir.path(), 2);
        let next = writer.next_seq();
        drop(writer);
        create_raw_segment(dir.path(), next + 100, SMALL_SEGMENT);
        let mut reader = open(dir.path(), kind);
        fault::clear();
        let path = dir.path().to_path_buf();
        fault::run_at(Point::ReaderEmptySlot, 0, move || {
            drop(Writer::open(&path, small_config()).unwrap());
        });
        let (_, end) = drain(&mut reader);
        assert!(end.is_ok(), "{end:?}");
        assert_eq!(segments(dir.path()).len(), 2);
        fault::clear();
    }
}

/// What a power loss can leave: a later segment, and not the records before it. Reported, as `verify` does, until a writer
/// has opened the journal; then the reader goes on.
#[test]
fn a_gap_left_by_a_power_loss_is_reported_until_it_is_repaired() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = journal(dir.path(), 2);
    writer.checkpoint().unwrap();
    append_n(&mut writer, 3);
    let next = writer.next_seq();
    drop(writer);
    create_raw_segment(dir.path(), next + 100, SMALL_SEGMENT);
    let path = segments(dir.path())[1].1.clone();
    let (at, _) = end_of_records(&path);
    let mut readers: Vec<_> = VISIBLE.map(|kind| open(dir.path(), kind)).into();
    for reader in &mut readers {
        expect_report(reader, &path, at, next, "gap");
    }
    // Nothing covers it for a durable-only reader.
    let mut durable = at_the_end(dir.path(), &[(false, true), (true, true)]);
    expect_wait(&mut durable, "gap");
    assert!(!inspect::verify(dir.path()).unwrap().problems.is_empty());
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 2);
    for reader in &mut readers {
        let (records, end) = drain(reader);
        assert!(end.is_ok(), "{end:?}");
        assert_eq!(records.len(), 2);
    }
}

/// Retention removes the segment, or recovery replaces it, in front of the check or inside it: `Discarded`, as before.
#[test]
fn a_segment_that_is_not_in_place_is_discarded() {
    for removed in [false, true] {
        for point in [
            Point::ReaderEmptySlotCheck,
            Point::ReaderEmptySlot,
            Point::ReaderEmptySlotConfirm,
        ] {
            for kind in KINDS {
                let case = format!("{point:?}, {kind:?}, removed {removed}");
                let dir = tempfile::tempdir().unwrap();
                let mut writer = journal(dir.path(), 4);
                writer.checkpoint().unwrap();
                let seq = 7;
                let (path, at) = locate_record(dir.path(), seq);
                zero(&path, at, 4);
                let mut reader = open(dir.path(), kind);
                fault::clear();
                let (journal, file) = (dir.path().to_path_buf(), path.clone());
                fault::run_at(point, 0, move || {
                    if removed {
                        let plan =
                            retention::plan(&journal, &Policy::default().max_segments(1)).unwrap();
                        retention::apply(&journal, &plan, &Action::Delete).unwrap();
                    } else {
                        let image = std::fs::read(&file).unwrap();
                        std::fs::write(journal.join("copy"), image).unwrap();
                        std::fs::rename(journal.join("copy"), &file).unwrap();
                    }
                });
                let (records, end) = drain(&mut reader);
                assert_eq!(records.len() as u64, seq, "{case}");
                assert!(
                    matches!(end, Err(Error::Discarded { seq: 7, .. })),
                    "{case}: {end:?}"
                );
                fault::clear();
                if removed {
                    assert!(matches!(
                        reader.seek(seq),
                        Err(Error::SeqUnavailable { .. })
                    ));
                } else {
                    reader.seek(seq).unwrap();
                    expect_report(&mut reader, &path, at, seq, &case);
                }
            }
        }
    }
}

fn append_tagged(writer: &mut Writer, n: u64, tag: u32, len: usize) {
    for _ in 0..n {
        writer
            .append(&NewRecord {
                schema_id: tag,
                venue_ts_ns: writer.next_seq(),
                local_ts_ns: 0,
                payload: &vec![0; len],
            })
            .unwrap();
    }
}

/// A repair in place under a visible reader that has read the removed records: its cursor starts no record of the new
/// history, whose payloads are zeros. Once the checkpoint passes it, it is told to seek; nothing is called corrupt.
#[test]
fn a_cursor_off_the_chain_is_discarded_not_corrupt() {
    for kind in VISIBLE {
        for rolled in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut writer = journal(dir.path(), 1);
            writer.checkpoint().unwrap();
            append_n(&mut writer, 6);
            let end = writer.next_seq();
            drop(writer);
            let mut readers = at_the_end(dir.path(), &[kind]);
            zero_commit_words_from(dir.path(), end - 3);
            let mut writer = Writer::open(dir.path(), small_config()).unwrap();
            assert_eq!(writer.next_seq(), end - 3);
            expect_wait(&mut readers, "repaired");
            // One record over the reader's place, and more.
            append_tagged(&mut writer, 5, 99, 2_000);
            if rolled {
                append_tagged(&mut writer, 80, 99, 2_000);
            }
            let reader = &mut readers[0];
            if !rolled {
                // No evidence yet: it waits, as it did before this check.
                assert!(reader.try_next().unwrap().is_none());
                writer.checkpoint().unwrap();
            }
            std::thread::sleep(Duration::from_millis(1_200));
            for _ in 0..2 {
                match reader.try_next() {
                    Err(Error::Discarded { seq, .. }) => assert_eq!(seq, end),
                    other => panic!("{kind:?}: {other:?}"),
                }
            }
            reader.seek(end).unwrap();
            let (records, result) = drain(reader);
            assert!(result.is_ok(), "{result:?}");
            assert_eq!(records[0].seq, end);
            assert!(records.iter().all(|r| r.schema_id == 99));
            assert_eq!(reader.position(), writer.next_seq());
        }
    }
}

/// A zeroed stretch under a reader, the record it passed last included: it cannot tell, seeks, and reports the first
/// record that is missing.
#[test]
fn a_zeroed_stretch_under_a_reader_is_reported_after_a_seek() {
    for kind in KINDS {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = journal(dir.path(), 1);
        writer.checkpoint().unwrap();
        // So that the safe reader has buffered nothing ahead.
        let mut reader = open(dir.path(), kind);
        for _ in 0..10 {
            reader.try_next().unwrap().unwrap();
        }
        let (path, from) = locate_record(dir.path(), 8);
        let (_, to) = locate_record(dir.path(), 14);
        zero(&path, from, (to - from) as usize);
        // Past what the safe reader has buffered: it has not looked at the rest yet.
        let end = loop {
            match reader.try_next() {
                Ok(Some(_)) => {}
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        match end {
            // The safe reader had everything in its buffer.
            Ok(()) => assert!(!kind.0 && reader.position() == writer.next_seq()),
            Err(Error::Discarded { seq: 10, .. }) => assert!(kind.0),
            Err(e) => panic!("{kind:?}: {e:?}"),
        }
        reader.seek(10).unwrap();
        expect_report(&mut reader, &path, from, 10, &format!("{kind:?}"));
    }
}

/// `seek_to_end` places a reader at the marker, behind no record that it has passed: it is at a record's start all the
/// same, and the record there is reported when it is emptied.
#[test]
fn a_reader_at_the_marker_after_seek_to_end_reports_the_slot() {
    for kind in KINDS {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = journal(dir.path(), 1);
        writer.checkpoint().unwrap();
        let seq = writer.next_seq();
        let mut reader = open(dir.path(), kind);
        reader.seek_to_end().unwrap();
        append_n(&mut writer, 5);
        writer.checkpoint().unwrap();
        let (path, at) = locate_record(dir.path(), seq);
        zero(&path, at, 4);
        std::thread::sleep(CHECK);
        let deadline = Instant::now() + Duration::from_secs(5);
        let end = loop {
            match reader.try_next() {
                Ok(None) if Instant::now() < deadline => {}
                Ok(r) => panic!("{kind:?}: {}", r.is_some()),
                Err(e) => break Err(e),
            }
        };
        assert!(is_report(&end, &path, at), "{kind:?}: {end:?}");
        expect_report(&mut reader, &path, at, seq, &format!("{kind:?}"));
    }
}

/// Landing through a seek index, and the next record emptied.
#[test]
fn a_reader_that_landed_through_an_index_reports_the_slot() {
    let dir = tempfile::tempdir().unwrap();
    let config = tickertape::WriterConfig::default().segment_size(1 << 20);
    let mut writer = Writer::open(dir.path(), config).unwrap();
    while segments(dir.path()).len() < 3 {
        append_n(&mut writer, 100);
    }
    writer.checkpoint().unwrap();
    tickertape::seek_index::build(dir.path(), false).unwrap();
    let last = segments(dir.path())[1].0 - 1;
    let (path, at) = locate_record(dir.path(), last);
    zero(&path, at, 4);
    for kind in KINDS {
        let config = ReaderConfig::default()
            .durable_only(kind.1)
            .notify(false)
            .seek_index(true);
        let mut reader = if kind.0 {
            // SAFETY: only this test changes the files, and no payload is held.
            unsafe { Reader::open_mapped(dir.path(), config) }.unwrap()
        } else {
            Reader::open(dir.path(), config).unwrap()
        };
        reader.seek(last).unwrap();
        assert!(reader.seek_used_index(), "{kind:?}");
        expect_report(&mut reader, &path, at, last, &format!("{kind:?}"));
    }
}

/// How quickly a slot is reported (the bounds stated in ADR 0050, with room for a loaded machine): at once at the first
/// empty slot after opening; otherwise 100 ms after arriving there if the marker shows it, and 1.1 s if only the segments
/// do. And how seldom a spinning reader looks.
#[test]
fn the_delay_and_the_number_of_checks_are_bounded() {
    let room = Duration::from_secs(3);
    for kind in VISIBLE {
        for marker in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let mut writer = journal(dir.path(), 1);
            if !marker {
                std::fs::remove_file(dir.path().join("checkpoint")).unwrap();
            }
            let mut reader = at_the_end(dir.path(), &[kind]).pop().unwrap();
            let checks = Rc::new(Cell::new(0));
            fn count(checks: Rc<Cell<u32>>) {
                fault::run_at(Point::ReaderEmptySlotCheck, 0, move || {
                    checks.set(checks.get() + 1);
                    count(checks);
                });
            }
            fault::clear();
            count(checks.clone());
            let start = Instant::now();
            let mut polls = 0u64;
            while start.elapsed() < Duration::from_millis(350) {
                assert!(reader.try_next().unwrap().is_none());
                polls += 1;
            }
            let most = start.elapsed().as_millis() / CHECK.as_millis() + 1;
            assert!(
                u128::from(checks.get()) <= most,
                "{} checks, {polls} polls",
                checks.get()
            );
            assert!(polls > 1_000, "{polls} polls");
            fault::clear();
            let seq = writer.next_seq();
            append_n(&mut writer, 400);
            if marker {
                writer.checkpoint().unwrap();
            }
            assert!(segments(dir.path()).len() > 1);
            let (path, at) = locate_record(dir.path(), seq + 3);
            zero(&path, at, 4);
            for expected in seq..seq + 3 {
                assert_eq!(reader.try_next().unwrap().unwrap().seq, expected);
            }
            // The safe reader's buffer ends nowhere near here only if it was filled before: it was not.
            let start = Instant::now();
            let end = loop {
                match reader.try_next() {
                    Ok(None) => {}
                    Ok(Some(r)) => panic!("{}", r.seq),
                    Err(e) => break Err(e),
                }
            };
            let took = start.elapsed();
            assert!(is_report(&end, &path, at), "{end:?}");
            let bound = if marker {
                Duration::from_millis(150)
            } else {
                Duration::from_millis(1_150)
            };
            assert!(took >= Duration::from_millis(150), "{took:?}");
            assert!(took < bound + room, "{took:?}");
            // At once, for a reader opened now.
            let mut reader = open(dir.path(), kind);
            let (_, end) = drain(&mut reader);
            assert!(is_report(&end, &path, at), "{end:?}");
        }
    }
}

/// The directory is listed only if it has changed since it was found to show nothing: a journal can have many thousand
/// segments. A segment linked later is seen within the same bound.
#[test]
fn an_unchanged_directory_is_not_listed_again() {
    fn count(listings: Rc<Cell<u32>>) {
        fault::run_at(Point::ReaderEmptySlotList, 0, move || {
            listings.set(listings.get() + 1);
            count(listings);
        });
    }
    let idle = |reader: &mut Reader, time: Duration| {
        let start = Instant::now();
        while start.elapsed() < time {
            assert!(reader.try_next().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    for kind in VISIBLE {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = journal(dir.path(), 2);
        let listings = Rc::new(Cell::new(0));
        fault::clear();
        count(listings.clone());
        // Modified just now: another change might not show. Listed at most once a second.
        let mut reader = at_the_end(dir.path(), &[kind]).pop().unwrap();
        idle(&mut reader, Duration::from_millis(1_400));
        assert!((2..=3).contains(&listings.get()), "{}", listings.get());
        std::thread::sleep(Duration::from_millis(1_000));
        idle(&mut reader, Duration::from_millis(1_200));
        let settled = listings.get();
        idle(&mut reader, Duration::from_millis(2_500));
        assert_eq!(listings.get(), settled, "{kind:?}");
        // A roll, followed by the reader, and a checkpoint: nothing that shows damage.
        while segments(dir.path()).len() < 3 {
            append_n(&mut writer, 10);
        }
        writer.checkpoint().unwrap();
        assert!(drain(&mut reader).1.is_ok());
        idle(&mut reader, Duration::from_millis(1_200));
        assert!(listings.get() > settled, "{kind:?}");
        // The directory settles again, and a later segment appears.
        std::thread::sleep(Duration::from_millis(2_100));
        idle(&mut reader, Duration::from_millis(1_200));
        create_raw_segment(dir.path(), writer.next_seq() + 100, SMALL_SEGMENT);
        let start = Instant::now();
        let end = loop {
            match reader.try_next() {
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Ok(Some(r)) => panic!("{}", r.seq),
                Err(e) => break Err(e),
            }
            assert!(start.elapsed() < Duration::from_secs(4), "{kind:?}");
        };
        let path = segments(dir.path())[2].1.clone();
        let at = end_of_records(&path).0;
        assert!(is_report(&end, &path, at), "{kind:?}: {end:?}");
        fault::clear();
    }
}

#[derive(Debug, Clone)]
enum Op {
    Append {
        n: u64,
        len: usize,
    },
    Checkpoint,
    /// Each reader reads this many records, after a seek to its position if `seek` (which makes it check at once).
    Read {
        n: usize,
        seek: bool,
    },
    /// Seeks to this many thousandths of the way from the oldest sequence number to a little past the newest.
    Seek {
        permille: u64,
    },
    ToEnd,
    Restart,
    /// A kill in an append: the bytes of a body and no commit word. Then readers read, and a writer opens.
    Torn {
        len: usize,
    },
    /// A kill between link and seal. Then readers read, and a writer opens.
    Linked,
    /// A marker that is missing, unreadable, or set back; readers read; it comes back.
    Marker {
        how: u8,
    },
    RepairInPlace {
        lost: u64,
        len: usize,
    },
    RepairByReplacement {
        lost: u64,
        len: usize,
    },
    Retain(usize),
    /// The commit word of a record (thousandths of the way through), or of a seal, is zeroed. Every reader must report
    /// it, if and only if the rule says so. Then it comes back.
    Damage {
        permille: u64,
        seal: bool,
    },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        8 => (1u64..60, 0usize..1500).prop_map(|(n, len)| Op::Append { n, len }),
        4 => Just(Op::Checkpoint),
        8 => (1usize..80, any::<bool>()).prop_map(|(n, seek)| Op::Read { n, seek }),
        3 => (0u64..1050).prop_map(|permille| Op::Seek { permille }),
        1 => Just(Op::ToEnd),
        2 => Just(Op::Restart),
        2 => (0usize..300).prop_map(|len| Op::Torn { len }),
        2 => Just(Op::Linked),
        1 => (0u8..3).prop_map(|how| Op::Marker { how }),
        2 => (1u64..20, 0usize..300).prop_map(|(lost, len)| Op::RepairInPlace { lost, len }),
        1 => (1u64..20, 0usize..300).prop_map(|(lost, len)| Op::RepairByReplacement { lost, len }),
        1 => (1usize..4).prop_map(Op::Retain),
        2 => (0u64..1000, any::<bool>()).prop_map(|(permille, seal)| Op::Damage { permille, seal }),
    ]
}

/// The number of cases: the default, or `TICKERTAPE_EMPTY_SLOT_CASES`.
fn cases() -> u32 {
    std::env::var("TICKERTAPE_EMPTY_SLOT_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24)
}

/// Random appends, rolls, checkpoints, kills, repairs (in place and by replacement) and removals with live readers of
/// every kind: none reports an empty slot. And zeroed commit words of records and seals: each is reported exactly when
/// the rule says so, at its place, by every kind of reader, fresh or in use.
#[test]
fn live_readers_report_exactly_the_damage() {
    let counts = [const { AtomicU64::new(0) }; 4];
    let mut runner = TestRunner::new(Config {
        cases: cases(),
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(&proptest::collection::vec(op(), 1..60), |ops| {
            run_ops(&ops, &counts);
            Ok(())
        })
        .unwrap();
    let [checked, reported, silent, discarded] = counts.map(AtomicU64::into_inner);
    eprintln!(
        "{} cases: {checked} reads to an end without damage, {reported} reports and {silent} undetectable ends with \
         damage, {discarded} discards",
        cases()
    );
    assert!(checked > 100 && reported > 10 && silent > 0);
}

/// Reads up to `n` records. Nothing is damaged: no empty slot may be reported. What a repair leaves a visible reader with
/// (`Discarded`, and other corruption at a cursor off the chain) is cleared by a seek, as an application would.
fn read_undamaged(
    reader: &mut Reader,
    kind: (bool, bool),
    n: usize,
    repaired: bool,
    counts: &[AtomicU64; 4],
    case: &str,
) {
    for _ in 0..n {
        match reader.next_owned() {
            Ok(Some(_)) => {}
            Ok(None) => {
                counts[0].fetch_add(1, Ordering::Relaxed);
                return;
            }
            Err(Error::Discarded { seq, .. }) => {
                assert!(repaired, "{case}, {kind:?}: discarded at {seq}");
                counts[3].fetch_add(1, Ordering::Relaxed);
                match reader.seek(seq) {
                    Ok(()) => {}
                    Err(Error::SeqUnavailable { first, .. }) => reader.seek(first).unwrap(),
                    Err(e) => panic!("{case}, {kind:?}: {e:?}"),
                }
            }
            Err(Error::Corrupt {
                reason: CorruptReason::EmptyCommitWord(_),
                path,
                offset,
                ..
            }) => panic!(
                "{case}, {kind:?}: false report at {}:{offset}",
                path.display()
            ),
            // Stale bytes at a cursor off the chain (C11), only for visible readers.
            Err(Error::Corrupt { reason, .. }) => {
                assert!(repaired && !kind.1, "{case}, {kind:?}: {reason:?}");
                reader.seek(reader.position()).unwrap();
            }
            Err(e) => panic!("{case}, {kind:?}: {e:?}"),
        }
    }
}

fn run_ops(ops: &[Op], counts: &[AtomicU64; 4]) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let marker = dir.join("checkpoint");
    let mut writer = Some(Writer::open(dir, small_config()).unwrap());
    append_n(writer.as_mut().unwrap(), 20);
    let mut readers = KINDS.map(|kind| open(dir, kind));
    // Whether anything that replaces or removes history has happened, and in place.
    let mut repaired = false;
    let mut tag = 100;
    for (step, op) in ops.iter().enumerate() {
        let case = format!("step {step}, {op:?}");
        // With no writer open, everything that readers do to wait is exercised by the operation itself.
        let mut read_all = |repaired: bool, case: &str| {
            for (reader, kind) in readers.iter_mut().zip(KINDS) {
                match reader.seek(reader.position()) {
                    Ok(()) => {}
                    Err(Error::SeqUnavailable { first, .. }) => reader.seek(first).unwrap(),
                    Err(e) => panic!("{case}: {e:?}"),
                }
                read_undamaged(reader, kind, usize::MAX, repaired, counts, case);
                let mut fresh = open(dir, kind);
                read_undamaged(&mut fresh, kind, usize::MAX, repaired, counts, case);
            }
        };
        let w = writer.as_mut().unwrap();
        match *op {
            Op::Append { n, len } => {
                for i in 0..n {
                    append_tagged(w, 1, tag, (len * (i as usize % 3)) / 2);
                }
            }
            Op::Checkpoint => w.checkpoint().unwrap(),
            Op::Read { n, seek } => {
                for (reader, kind) in readers.iter_mut().zip(KINDS) {
                    if seek {
                        match reader.seek(reader.position()) {
                            Ok(()) => {}
                            Err(Error::SeqUnavailable { first, .. }) => reader.seek(first).unwrap(),
                            Err(e) => panic!("{case}: {e:?}"),
                        }
                    }
                    read_undamaged(reader, kind, n, repaired, counts, &case);
                }
            }
            Op::Seek { permille } => {
                let first = segments(dir)[0].0;
                let target = first + (w.next_seq() - first) * permille / 1000;
                for (reader, kind) in readers.iter_mut().zip(KINDS) {
                    reader.seek(target).unwrap();
                    read_undamaged(reader, kind, 5, repaired, counts, &case);
                }
            }
            Op::ToEnd => {
                for reader in &mut readers {
                    reader.seek_to_end().unwrap();
                    assert!(reader.try_next().unwrap().is_none());
                }
            }
            Op::Restart => {
                drop(writer.take());
                writer = Some(Writer::open(dir, small_config()).unwrap());
                read_all(repaired, &case);
            }
            Op::Torn { len } => {
                drop(writer.take());
                let path = segments(dir).pop().unwrap().1;
                let (at, _) = end_of_records(&path);
                let room = (SMALL_SEGMENT - at - 4) as usize;
                let torn = vec![0x5a; len.min(room)];
                open_raw(&path).write_all_at(&torn, at + 4).unwrap();
                read_all(repaired, &case);
                let next = end_of_records(&path).1;
                writer = Some(Writer::open(dir, small_config()).unwrap());
                read_all(repaired, &case);
                assert_eq!(writer.as_ref().unwrap().next_seq(), next);
            }
            Op::Linked => {
                let next = w.next_seq();
                drop(writer.take());
                if segments(dir).last().unwrap().0 != next {
                    create_raw_segment(dir, next, SMALL_SEGMENT);
                }
                read_all(repaired, &case);
                writer = Some(Writer::open(dir, small_config()).unwrap());
                read_all(repaired, &case);
            }
            Op::Marker { how } => {
                let image = std::fs::read(&marker).unwrap();
                std::fs::remove_file(&marker).unwrap();
                match how {
                    0 => {}
                    1 => std::fs::create_dir(&marker).unwrap(),
                    _ => {
                        let mut older = image.clone();
                        older[65_536..65_600].fill(0);
                        std::fs::write(&marker, older).unwrap();
                    }
                }
                for (reader, kind) in readers.iter_mut().zip(KINDS) {
                    // A durable-only reader reports a marker it cannot read.
                    if !kind.1 {
                        read_undamaged(reader, kind, usize::MAX, repaired, counts, &case);
                        read_undamaged(
                            &mut open(dir, kind),
                            kind,
                            usize::MAX,
                            repaired,
                            counts,
                            &case,
                        );
                    }
                }
                let _ = std::fs::remove_dir(&marker);
                let _ = std::fs::remove_file(&marker);
                // The writer keeps its marker file open: the same inode must come back, so it opens the journal again.
                drop(writer.take());
                std::fs::write(&marker, image).unwrap();
                writer = Some(Writer::open(dir, small_config()).unwrap());
            }
            Op::RepairInPlace { lost, len } | Op::RepairByReplacement { lost, len } => {
                let newest = segments(dir).last().unwrap().0;
                let covered = inspect::read_checkpoint(dir)
                    .unwrap()
                    .map_or(0, |c| c.next_seq);
                let next = w.next_seq();
                let at = next.saturating_sub(lost).max(covered).max(newest);
                if at >= next {
                    continue;
                }
                drop(writer.take());
                if matches!(op, Op::RepairByReplacement { .. }) {
                    corrupt_by_replacement(dir, at);
                } else {
                    zero_commit_words_from(dir, at);
                }
                let mut w = Writer::open(dir, small_config()).unwrap();
                assert_eq!(w.next_seq(), at);
                tag += 1;
                repaired = true;
                append_tagged(&mut w, lost + 3, tag, len);
                writer = Some(w);
            }
            Op::Retain(keep) => {
                let plan = retention::plan(dir, &Policy::default().max_segments(keep)).unwrap();
                retention::apply(dir, &plan, &Action::Delete).unwrap();
                repaired = true;
            }
            Op::Damage { permille, seal } => {
                damage(dir, &mut readers, permille, seal, counts, &case)
            }
        }
    }
}

/// Zeroes a commit word and has every reader read up to it.
fn damage(
    dir: &Path,
    readers: &mut [Reader; 4],
    permille: u64,
    seal: bool,
    counts: &[AtomicU64; 4],
    case: &str,
) {
    let listed = segments(dir);
    let newest = listed.last().unwrap().clone();
    let end = end_of_records(&newest.1).1;
    let first = listed[0].0;
    let mut target = first + (end - first) * permille / 1000;
    let (path, at) = if seal {
        if listed.len() < 2 {
            return;
        }
        let i = (permille as usize) % (listed.len() - 1);
        target = listed[i + 1].0;
        (listed[i].1.clone(), end_of_records(&listed[i].1).0)
    } else if target < end {
        locate_record(dir, target)
    } else {
        return;
    };
    let own = listed.iter().find(|s| s.1 == path).unwrap().0;
    let current = inspect::read_checkpoint(dir).unwrap();
    let by_marker = current.is_some_and(|c| c.next_seq > target || c.segment_base_seq > own);
    // The newest segment of a pair that a seal joins is evidence if it holds a record.
    let by_segment = newest.0 > target || (seal && newest.0 == target && end > newest.0);
    let durable_end = current.map_or(0, |c| c.next_seq);
    let mut word = [0; 4];
    open_raw(&path).read_exact_at(&mut word, at).unwrap();
    zero(&path, at, 4);
    let start = own.max(first);
    for (reader, kind) in readers.iter_mut().zip(KINDS) {
        let mut fresh = open(dir, kind);
        for (reader, which) in [(reader, "in use"), (&mut fresh, "fresh")] {
            let case = format!("{case}, {which} {kind:?}, {target} in {own}");
            reader.seek(start).unwrap();
            let (records, result) = drain(reader);
            let expected = if kind.1 {
                by_marker && (target < durable_end || current.unwrap().segment_base_seq > own)
            } else {
                by_marker || by_segment
            };
            if expected {
                assert!(is_report(&result, &path, at), "{case}: {result:?}");
                let again = reader.try_next().map(|_| ());
                assert!(is_report(&again, &path, at), "{case}: {again:?}");
                counts[1].fetch_add(1, Ordering::Relaxed);
            } else {
                assert!(result.is_ok(), "{case}: {result:?}");
                counts[2].fetch_add(1, Ordering::Relaxed);
            }
            let seqs: Vec<u64> = records.iter().map(|r| r.seq).collect();
            let to = if kind.1 {
                target.min(durable_end)
            } else {
                target
            };
            assert_eq!(seqs, (start..to.max(start)).collect::<Vec<_>>(), "{case}");
        }
    }
    open_raw(&path).write_all_at(&word, at).unwrap();
}

fn rounds(default: u64) -> u64 {
    std::env::var("TICKERTAPE_EMPTY_SLOT_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// A live writer process, killed and restarted again and again, with retention, and readers of every kind in threads:
/// spinning, waiting with the back-off, reopening, seeking. No damage is injected, so no reader may report any.
#[test]
fn live_readers_of_a_killed_writer_see_no_false_error() {
    killed_writer(rounds(12));
}

#[test]
#[ignore = "long"]
fn live_readers_of_a_killed_writer_see_no_false_error_long() {
    killed_writer(rounds(250));
}

fn killed_writer(rounds: u64) {
    let root = tempfile::tempdir().unwrap();
    let dir: PathBuf = root.path().join("j");
    let mut rng = Rng::from_env("live_readers_of_a_killed_writer_see_no_false_error");
    drop(Writer::open(&dir, small_config()).unwrap());
    let stop = AtomicBool::new(false);
    let (ends, records, discards) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    std::thread::scope(|s| {
        let (dir, stop, ends, records, discards) = (&dir, &stop, &ends, &records, &discards);
        // Stops the threads also when this one panics.
        struct Stop<'a>(&'a AtomicBool);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let _stop = Stop(stop);
        for kind in KINDS {
            for style in 0..3 {
                s.spawn(move || {
                    let mut reader = open(dir, kind);
                    let mut expected = reader.position();
                    let mut n = 0u64;
                    while !stop.load(Ordering::SeqCst) {
                        let next = match style {
                            0 => reader.try_next(),
                            1 => reader.next_timeout(Some(Duration::from_millis(400))),
                            _ => reader.next_timeout(Some(Duration::from_millis(1))),
                        };
                        match next {
                            Ok(Some(r)) => {
                                assert_eq!(r.seq, expected, "{kind:?}, {style}");
                                assert!(
                                    r.payload == payload(r.seq, kill_len(r.seq)),
                                    "{kind:?}, {style}: payload of {}",
                                    r.seq
                                );
                                expected += 1;
                                n += 1;
                            }
                            Ok(None) => {
                                ends.fetch_add(1, Ordering::Relaxed);
                                if style == 2 {
                                    // Every check from the start: a new reader, or one just moved.
                                    if n % 2 == 0 {
                                        reader = open(dir, kind);
                                        if reader.position() > expected {
                                            discards.fetch_add(1, Ordering::Relaxed);
                                            expected = reader.position();
                                        }
                                    }
                                    match reader.seek(expected) {
                                        Ok(()) => {}
                                        Err(Error::SeqUnavailable { first, .. }) => {
                                            expected = first;
                                            // It can be gone by now, and is sought again then.
                                            let _ = reader.seek(first);
                                        }
                                        Err(e) => panic!("{kind:?}, {style}: {e:?}"),
                                    }
                                }
                            }
                            // Only retention removes anything.
                            Err(Error::Discarded { seq, .. }) => {
                                assert_eq!(seq, expected);
                                discards.fetch_add(1, Ordering::Relaxed);
                                let mut target = seq;
                                while let Err(e) = reader.seek(target) {
                                    match e {
                                        Error::SeqUnavailable { first, .. } => target = first,
                                        e => panic!("{kind:?}, {style}: {e:?}"),
                                    }
                                }
                                expected = target;
                            }
                            Err(e) => panic!("{kind:?}, {style}, at {expected}: {e:?}"),
                        }
                    }
                    records.fetch_add(n, Ordering::Relaxed);
                });
            }
        }
        for round in 0..rounds {
            let mut child = Command::new(env!("CARGO_BIN_EXE_tt-kill-writer"))
                .args([
                    dir.to_str().unwrap(),
                    &SMALL_SEGMENT.to_string(),
                    &rng.next_u64().to_string(),
                ])
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            let until = Instant::now() + Duration::from_millis(20 + rng.below(250));
            while Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
                let policy = Policy::default().max_segments(6);
                if let Ok(plan) = retention::plan(dir, &policy) {
                    let _ = retention::apply(dir, &plan, &Action::Delete);
                }
            }
            child.kill().unwrap();
            child.wait().unwrap();
            // Waiting readers check at 100 ms, and list the segments after 1 s.
            if round % 4 == 0 {
                std::thread::sleep(Duration::from_millis(rng.below(1_300)));
            }
        }
    });
    let problems = inspect::verify(&dir).unwrap().problems;
    let torn_or_linked: BTreeSet<_> = problems.iter().map(|p| format!("{:?}", p.reason)).collect();
    eprintln!(
        "{rounds} rounds: {} records read, {} reads to an end, {} discards; verify at the end: {torn_or_linked:?}",
        records.into_inner(),
        ends.into_inner(),
        discards.into_inner()
    );
}
