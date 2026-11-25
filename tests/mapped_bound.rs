//! The checkpoint marker as the bound on the zero-copy reader's loads (ADR 0048): damaged commit words under a live
//! writer, and the same results as the safe reader whatever happens to the marker. The tests that look at the loads
//! themselves are in `crates/tickertape/src/reader_bound_tests.rs`.

use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use tickertape::retention::{self, Action, Policy};
use tickertape::{
    Error, NewRecord, OwnedRecord, Reader, ReaderConfig, RecoveryAction, Writer, format,
};
use tickertape_tests::{
    append_n, bounded_len, check_record, corrupt_by_replacement, end_of_records, locate_record,
    open_raw, payload, segments, small_config, zero_commit_words_from,
};

/// The kinds of reader: (mapped, durable-only).
const KINDS: [(bool, bool); 4] = [(false, false), (true, false), (false, true), (true, true)];

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

/// Up to `limit` records from the cursor, and how reading ended ("more": the limit).
fn read_up_to(reader: &mut Reader, limit: usize) -> (Vec<OwnedRecord>, String) {
    let mut records = Vec::new();
    while records.len() < limit {
        match reader.next_owned() {
            Ok(Some(record)) => records.push(record),
            Ok(None) => return (records, "end".into()),
            Err(e) => return (records, format!("{e:?}")),
        }
    }
    (records, "more".into())
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

fn write_word(path: &Path, at: u64, word: u32) {
    open_raw(path)
        .write_all_at(&word.to_le_bytes(), at)
        .unwrap();
}

/// Enlarges the length of record `seq` so that its frame ends `past` bytes after the last record.
fn enlarge(dir: &Path, seq: u64, past: u64) -> u64 {
    let (path, at) = locate_record(dir, seq);
    let (end, _) = end_of_records(&path);
    write_word(&path, at, (end - at + past) as u32);
    at
}

/// The safe readers' buffers may hold the bytes from before some damage: seeks drop them.
fn reread(readers: &mut [Reader; 4], position: u64) {
    for reader in readers {
        reader.seek(position).unwrap();
    }
}

/// The reader is at a record with a damaged length: it reports a checksum mismatch there, every time.
fn expect_bad_checksum(reader: &mut Reader, at: u64, times: usize, case: &str) {
    for _ in 0..times {
        match reader.next_owned() {
            Err(Error::Corrupt {
                offset,
                reason: tickertape::CorruptReason::BadChecksum,
                ..
            }) => assert_eq!(offset, at, "{case}"),
            other => panic!("{case}: {other:?}"),
        }
    }
}

/// Test plan 5, a covered length: it survives `Writer::open` (no recovery action), and every reader, while a thread or
/// another process appends where the frame would reach, reports what the safe reader reports.
#[test]
fn a_covered_enlarged_length_reads_the_same_under_a_live_writer() {
    for process in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("j");
        let config = tickertape::WriterConfig::default().segment_size(1 << 20);
        let mut writer = Writer::open(&journal, config.clone()).unwrap();
        let append = |writer: &mut Writer| {
            let seq = writer.next_seq();
            let data = payload(seq, bounded_len(seq, 100));
            writer
                .append(&NewRecord {
                    schema_id: (seq % 7) as u32,
                    venue_ts_ns: 0,
                    local_ts_ns: 0,
                    payload: &data,
                })
                .unwrap();
        };
        for _ in 0..60 {
            append(&mut writer);
        }
        writer.checkpoint().unwrap();
        drop(writer);
        let at = enlarge(&journal, 20, 40_000);

        let stop = AtomicBool::new(false);
        let go = dir.path().join("go");
        std::thread::scope(|s| {
            let mut child = None;
            if process {
                child = Some(
                    Command::new(env!("CARGO_BIN_EXE_tt-writer"))
                        .args([journal.to_str().unwrap(), "6000", "1048576", "100", "5999"])
                        .arg(&go)
                        .spawn()
                        .unwrap(),
                );
            } else {
                let mut writer = Writer::open(&journal, config).unwrap();
                let report = writer.recovery_report();
                assert!(report.actions.is_empty(), "{report:?}");
                let stop = &stop;
                s.spawn(move || {
                    while !stop.load(Ordering::SeqCst) && writer.next_seq() < 6_000 {
                        append(&mut writer);
                        std::thread::sleep(Duration::from_micros(100));
                    }
                });
            }
            let readers: Vec<_> = KINDS
                .into_iter()
                .map(|kind| {
                    let journal = &journal;
                    s.spawn(move || {
                        let mut reader = open(journal, kind);
                        let (records, _) = read_up_to(&mut reader, 20);
                        assert_eq!(records.len(), 20);
                        expect_bad_checksum(&mut reader, at, 4, &format!("{kind:?}"));
                    })
                })
                .collect();
            let results: Vec<_> = readers.into_iter().map(|r| r.join()).collect();
            stop.store(true, Ordering::SeqCst);
            std::fs::write(&go, b"").unwrap();
            if let Some(mut child) = child {
                assert!(child.wait().unwrap().success());
            }
            for result in results {
                result.unwrap();
            }
        });
    }
}

/// Test plan 5, an uncovered length: readers report it while a writer is open, and the next open repairs it by replacing
/// the file, so that readers of the old file get `Discarded` and new ones read everything there is.
#[test]
fn an_uncovered_enlarged_length_is_repaired_at_the_next_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 10);
    writer.checkpoint().unwrap();
    append_n(&mut writer, 20);
    let mut readers = KINDS.map(|kind| open(dir.path(), kind));
    for reader in &mut readers {
        assert_eq!(read_up_to(reader, 5).0.len(), 5);
    }
    // While the writer is open, and idle.
    let at = enlarge(dir.path(), 20, 2_000);
    reread(&mut readers, 5);
    for (reader, kind) in readers.iter_mut().zip(KINDS) {
        let (records, end) = read_up_to(reader, 100);
        for record in &records {
            check_record(record);
        }
        let position = 5 + records.len() as u64;
        if kind.1 {
            assert_eq!((position, end.as_str()), (10, "end"), "{kind:?}");
        } else {
            assert_eq!(position, 20, "{kind:?}");
            expect_bad_checksum(reader, at, 2, &format!("{kind:?}"));
        }
    }
    drop(writer);
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    let actions = writer.recovery_report().actions.clone();
    assert!(
        matches!(actions[..], [RecoveryAction::RepairedTail { .. }, ..]),
        "{actions:?}"
    );
    append_tagged(&mut writer, 5, 99, 100);
    writer.checkpoint().unwrap();
    let total = writer.next_seq();
    for (reader, kind) in readers.iter_mut().zip(KINDS) {
        let seq = if kind.1 { 10 } else { 20 };
        match reader.next_owned() {
            Err(Error::Discarded { seq: at, .. }) => assert_eq!(at, seq, "{kind:?}"),
            other => panic!("{kind:?}: {other:?}"),
        }
        let (records, end) = read_up_to(&mut open(dir.path(), kind), 1_000);
        assert_eq!((records.len() as u64, end.as_str()), (total, "end"));
    }
}

/// Test plan 5, the empty head slot: a length or a seal marker there, under an open writer. Visible readers of both kinds
/// report it, durable-only ones do not touch it, and the next append overwrites it. With the marker at the head, behind
/// it, and before the segment; the reader caught up, or not.
#[test]
fn a_damaged_head_slot_reads_the_same() {
    for (covered, word) in [
        (30, 1_920),
        (30, format::SEAL_MARKER),
        (10, 1_920),
        (0, 1_920),
    ] {
        for caught_up in [false, true] {
            let case = format!("covered {covered}, word {word:#x}, caught up {caught_up}");
            let dir = tempfile::tempdir().unwrap();
            let mut writer = Writer::open(dir.path(), small_config()).unwrap();
            append_n(&mut writer, covered);
            if covered > 0 {
                writer.checkpoint().unwrap();
            }
            append_n(&mut writer, 30 - covered);
            let mut readers = KINDS.map(|kind| open(dir.path(), kind));
            if caught_up {
                for reader in &mut readers {
                    read_up_to(reader, 100);
                }
            }
            let path = segments(dir.path())[0].1.clone();
            let (head, _) = end_of_records(&path);
            write_word(&path, head, word);
            let results = readers.each_mut().map(|reader| {
                let (records, end) = read_up_to(reader, 100);
                for record in &records {
                    check_record(record);
                }
                let position = reader.position();
                // Again: the same.
                let again = read_up_to(reader, 100).1;
                assert_eq!(end, again, "{case}");
                (position, end)
            });
            assert_eq!(results[0], results[1], "{case}");
            assert_eq!(results[2], results[3], "{case}");
            let reason = if word == format::SEAL_MARKER {
                "BadSeal"
            } else {
                "BadChecksum"
            };
            assert!(results[0].1.contains(reason), "{case}: {results:?}");
            assert_eq!(results[2], (covered, "end".to_string()), "{case}");
            append_n(&mut writer, 5);
            for (reader, kind) in readers.iter_mut().zip(KINDS) {
                let (records, end) = read_up_to(reader, 100);
                assert_eq!(end, "end", "{case}");
                assert_eq!(records.len(), if kind.1 { 0 } else { 5 }, "{case}");
                for record in &records {
                    check_record(record);
                }
            }
        }
    }
}

/// Test plan 7: the marker is lost, or an older one becomes current, and recovery then repairs in place what the lost
/// marker covered, under readers that have not read it yet. The zero-copy reader, whose bound now lies inside a record of
/// the new history, returns what the safe reader returns.
#[test]
fn a_bound_from_a_lost_marker_costs_a_copy_and_nothing_else() {
    for set_back in [false, true] {
        for (len, passed) in [(64, 0), (200, 0), (33, 3), (1_000, 12)] {
            let case = format!("set back {set_back}, length {len}, passed {passed}");
            let dir = tempfile::tempdir().unwrap();
            let mut writer = Writer::open(dir.path(), small_config()).unwrap();
            let marker = dir.path().join("checkpoint");
            append_tagged(&mut writer, 5, 1, 64);
            writer.checkpoint().unwrap();
            let older = std::fs::read(&marker).unwrap();
            append_tagged(&mut writer, 15, 1, 64);
            writer.checkpoint().unwrap();
            drop(writer);
            let mut readers = KINDS.map(|kind| open(dir.path(), kind));
            for reader in &mut readers {
                assert_eq!(read_up_to(reader, passed).0.len(), passed);
                // The safe reader's buffer is dropped: it is the zero-copy reader that is compared.
                reader.seek(passed as u64).unwrap();
            }
            if set_back {
                std::fs::write(&marker, older).unwrap();
            } else {
                std::fs::remove_file(&marker).unwrap();
            }
            zero_commit_words_from(dir.path(), 13);
            let mut writer = Writer::open(dir.path(), small_config()).unwrap();
            assert_eq!(writer.next_seq(), 13);
            append_tagged(&mut writer, 20, 2, len);
            for round in 0..2 {
                let results = readers.each_mut().map(|r| read_up_to(r, 1_000));
                assert_eq!(results[0], results[1], "{case}, round {round}");
                assert_eq!(results[2], results[3], "{case}, round {round}");
                let want = if round == 0 { 33 - passed } else { 0 };
                assert_eq!(results[0].0.len(), want, "{case}");
                writer.checkpoint().unwrap();
            }
        }
    }
}

/// Retention removes the segment while a visible reader is below its bound (C14): every record is returned, in a segment
/// that holds damage too, where the zero-copy reader stops with the safe reader's result.
#[test]
fn retention_below_the_bound() {
    for damaged in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut writer, 50);
        writer.checkpoint().unwrap();
        let mut readers = KINDS.map(|kind| open(dir.path(), kind));
        for reader in &mut readers {
            assert_eq!(read_up_to(reader, 5).0.len(), 5);
        }
        if damaged {
            enlarge(dir.path(), 40, 3_000);
            reread(&mut readers, 5);
        }
        append_n(&mut writer, 2_000);
        writer.checkpoint().unwrap();
        let second = segments(dir.path())[1].0;
        let plan = retention::plan(dir.path(), &Policy::default().max_segments(1)).unwrap();
        retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
        for durable in [0, 2] {
            let [safe, mapped] = [0, 1].map(|m| {
                let reader = &mut readers[durable + m];
                let (records, end) = read_up_to(reader, usize::MAX);
                for record in &records {
                    check_record(record);
                }
                (records, end)
            });
            assert_eq!(safe, mapped, "damaged {damaged}, {durable}");
            let end = safe.0.last().unwrap().seq + 1;
            assert!(safe.1.starts_with("Discarded"), "{}", safe.1);
            let want = if damaged { 40 } else { second };
            // A durable-only reader learns it when it looks for a newer marker (ADR 0034).
            assert_eq!(end, if durable > 0 { want.min(50) } else { want });
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Append {
        n: u64,
        len: usize,
    },
    Checkpoint,
    Read(usize),
    Seek(u64),
    ToEnd,
    RepairInPlace {
        lost: u64,
        len: usize,
    },
    RepairByReplacement {
        lost: u64,
        len: usize,
    },
    Restart,
    /// The marker is lost, set back to an older one or a valid one at an arbitrary place, before the writer opens again.
    LoseMarker(u8),
    /// The same, and the newest segment's records from this one on are lost, covered or not.
    LoseMarkerAndRecords {
        how: u8,
        lost: u64,
        len: usize,
    },
    /// A record's length is changed by this much, and put back after both kinds have read.
    DamageLength {
        back: u64,
        by: i64,
    },
    Retain(usize),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (1u64..40, 0usize..1500).prop_map(|(n, len)| Op::Append { n, len }),
        5 => Just(Op::Checkpoint),
        6 => (1usize..60).prop_map(Op::Read),
        3 => (0u64..1000).prop_map(Op::Seek),
        1 => Just(Op::ToEnd),
        1 => (1u64..20, 0usize..300).prop_map(|(lost, len)| Op::RepairInPlace { lost, len }),
        1 => (1u64..20, 0usize..300).prop_map(|(lost, len)| Op::RepairByReplacement { lost, len }),
        1 => Just(Op::Restart),
        1 => any::<u8>().prop_map(Op::LoseMarker),
        2 => (any::<u8>(), 1u64..30, 0usize..300)
            .prop_map(|(how, lost, len)| Op::LoseMarkerAndRecords { how, lost, len }),
        1 => (1u64..60, -200i64..20_000).prop_map(|(back, by)| Op::DamageLength { back, by }),
        1 => (1usize..4).prop_map(Op::Retain),
    ]
}

/// Any sequence of appends, rolls, checkpoints, repairs, removals, damaged lengths, and markers lost or moved back: a
/// zero-copy reader that does what a safe one does, returns the records and errors the safe one returns.
#[test]
fn the_bound_changes_no_result() {
    let compared = AtomicU64::new(0);
    let mut runner = TestRunner::new(Config {
        cases: 48,
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(&proptest::collection::vec(op(), 1..50), |ops| {
            run_ops(&ops, &compared);
            Ok(())
        })
        .unwrap();
    let compared = compared.into_inner();
    assert!(compared > 3_000, "only {compared} records compared");
}

/// Reads up to `n` records with each reader and compares the zero-copy readers' results with the safe readers'.
fn compare(readers: &mut [Reader; 4], n: usize, compared: &AtomicU64, case: &str) {
    let mut results = readers.each_mut().map(|r| read_up_to(r, n));
    for i in [0, 2] {
        // Whether a reader that waits at an empty slot has noticed a replaced file depends on the clock (100 ms).
        let idle = |end: &str| end == "end" || end.starts_with("Discarded");
        if idle(&results[i].1) && idle(&results[i + 1].1) {
            results[i + 1].1 = results[i].1.clone();
        }
        assert_eq!(results[i], results[i + 1], "{case}");
        compared.fetch_add(results[i].0.len() as u64, Ordering::Relaxed);
    }
}

/// Puts readers that a repair, or the clock, has put apart back together: every reader at the safe one's position.
fn align(readers: &mut [Reader; 4]) {
    for i in [0, 2] {
        let at = readers[i].position();
        let [safe, mapped] = [0, 1].map(|m| readers[i + m].seek(at).map_err(|e| format!("{e:?}")));
        assert_eq!(safe, mapped);
        if safe.is_err() {
            // Retention removed it, or recovery everything from there on.
            for m in 0..2 {
                readers[i + m].seek_to_end().unwrap();
            }
        }
    }
}

fn spoil_marker(dir: &Path, how: u8, older: &[u8]) {
    let path = dir.join("checkpoint");
    match how % 4 {
        0 => drop(std::fs::remove_file(&path)),
        1 if !older.is_empty() => std::fs::write(&path, older).unwrap(),
        // One slot damaged, so that the other one counts.
        _ => {
            if let Ok(mut marker) = std::fs::read(&path) {
                let at = format::CHECKPOINT_SLOT_OFFSETS[usize::from(how / 4 % 2)];
                if let Some(slot) = marker.get_mut(at + 20..at + 24) {
                    slot.fill(how);
                }
                std::fs::write(&path, marker).unwrap();
            }
        }
    }
}

fn run_ops(ops: &[Op], compared: &AtomicU64) {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let mut writer = Writer::open(dir, small_config()).unwrap();
    append_n(&mut writer, 20);
    let mut older = Vec::new();
    let mut readers = KINDS.map(|kind| open(dir, kind));
    let mut tag = 100;
    for op in ops {
        let case = format!("{op:?} of {ops:?}");
        match *op {
            Op::Append { n, len } => append_tagged(&mut writer, n, tag, len),
            Op::Checkpoint => {
                if tag % 3 == 0 || older.is_empty() {
                    older = std::fs::read(dir.join("checkpoint")).unwrap_or_default();
                }
                writer.checkpoint().unwrap();
            }
            Op::Read(n) => compare(&mut readers, n, compared, &case),
            Op::Seek(permille) => {
                let first = segments(dir)[0].0;
                let target = first + (writer.next_seq() - first) * permille / 1000;
                for reader in &mut readers {
                    reader.seek(target).unwrap();
                }
                compare(&mut readers, 10, compared, &case);
            }
            Op::ToEnd => {
                let ends = readers.each_mut().map(|r| {
                    r.seek_to_end()
                        .map_err(|e| format!("{e:?}"))
                        .map(|()| r.position())
                });
                assert_eq!(ends[0], ends[1], "{case}");
                assert_eq!(ends[2], ends[3], "{case}");
            }
            Op::Restart => {
                drop(writer);
                writer = Writer::open(dir, small_config()).unwrap();
            }
            Op::LoseMarker(how) => {
                drop(writer);
                spoil_marker(dir, how, &older);
                let Ok(opened) = Writer::open(dir, small_config()) else {
                    // It names a segment that retention has removed: the open refuses.
                    return;
                };
                writer = opened;
            }
            Op::RepairInPlace { lost, len }
            | Op::RepairByReplacement { lost, len }
            | Op::LoseMarkerAndRecords { lost, len, .. } => {
                let newest = segments(dir).last().unwrap().0;
                // The safe reader's buffer must not hold what is about to go: seek there again.
                align(&mut readers);
                drop(writer);
                if let Op::LoseMarkerAndRecords { how, .. } = op {
                    spoil_marker(dir, *how, &older);
                }
                // Only what no marker covers any more: the next open does not look at the rest.
                let covered = tickertape::inspect::read_checkpoint(dir)
                    .unwrap()
                    .map_or(0, |c| c.next_seq);
                let next = read_up_to(&mut open(dir, (false, false)), usize::MAX);
                assert_eq!(next.1, "end", "{case}");
                let next = next.0.last().map_or(0, |r| r.seq + 1);
                let at = next.saturating_sub(lost).max(covered).max(newest);
                if at >= next {
                    let Ok(opened) = Writer::open(dir, small_config()) else {
                        return;
                    };
                    writer = opened;
                    continue;
                }
                if matches!(op, Op::RepairByReplacement { .. }) {
                    corrupt_by_replacement(dir, at);
                } else {
                    zero_commit_words_from(dir, at);
                }
                let Ok(opened) = Writer::open(dir, small_config()) else {
                    return;
                };
                writer = opened;
                tag += 1;
                append_tagged(&mut writer, lost + 3, tag, len);
                compare(&mut readers, 1_000, compared, &case);
                align(&mut readers);
            }
            Op::DamageLength { back, by } => {
                let first = segments(dir)[0].0;
                let seq = writer.next_seq().saturating_sub(back).max(first);
                if seq == writer.next_seq() {
                    continue;
                }
                let (path, at) = locate_record(dir, seq);
                let mut word = [0; 4];
                open_raw(&path).read_exact_at(&mut word, at).unwrap();
                let damaged = i64::from(u32::from_le_bytes(word)) + by;
                for reader in &mut readers {
                    reader.seek(seq.saturating_sub(3).max(first)).unwrap();
                }
                write_word(&path, at, damaged.max(1) as u32);
                compare(&mut readers, 20, compared, &case);
                write_word(&path, at, u32::from_le_bytes(word));
                compare(&mut readers, 20, compared, &case);
                align(&mut readers);
            }
            Op::Retain(keep) => {
                let policy = Policy::default().max_segments(keep);
                let plan = retention::plan(dir, &policy).unwrap();
                retention::apply(dir, &plan, &Action::Delete).unwrap();
                compare(&mut readers, 1_000, compared, &case);
                align(&mut readers);
            }
        }
    }
    compare(
        &mut readers,
        usize::MAX,
        compared,
        &format!("at the end of {ops:?}"),
    );
}
