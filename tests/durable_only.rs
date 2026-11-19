//! Crash contract C13: readers expose only the checkpointed prefix.

use std::process::Command;
use std::time::Duration;

use tickertape::fault::Point;
use tickertape::retention::{self, Action, Policy};
use tickertape::{Error, NewRecord, OwnedRecord, Reader, ReaderConfig, RecoveryAction, Writer};
use tickertape_tests::{
    append_n, check_record, corrupt_by_replacement, read_all, reopen_writer, segments,
    small_config, zero_commit_word,
};

fn open(dir: &std::path::Path, mapped: bool) -> Reader {
    let config = ReaderConfig::default().durable_only(true);
    if mapped {
        // SAFETY: only the tickertape writer modifies segment files in these tests.
        unsafe { Reader::open_mapped(dir, config) }.unwrap()
    } else {
        Reader::open(dir, config).unwrap()
    }
}

fn expect_range(reader: &mut Reader, range: std::ops::Range<u64>) {
    for seq in range {
        let record = reader.next_owned().unwrap().expect("checkpointed record");
        assert_eq!(record.seq, seq);
        check_record(&record);
    }
    assert!(reader.try_next().unwrap().is_none());
}

#[test]
fn no_valid_checkpoint_returns_nothing() {
    assert!(!ReaderConfig::default().durable_only);
    assert!(
        !ReaderConfig::default()
            .durable_only(true)
            .durable_only(false)
            .durable_only
    );
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 10);
    for mapped in [false, true] {
        assert!(open(dir.path(), mapped).try_next().unwrap().is_none());
    }
    let marker = dir.path().join("checkpoint");
    std::fs::remove_file(&marker).unwrap();
    assert!(open(dir.path(), false).try_next().unwrap().is_none());
    std::fs::write(&marker, b"invalid marker").unwrap();
    assert!(open(dir.path(), false).try_next().unwrap().is_none());
    assert_eq!(read_all(dir.path()).len(), 10);
}

#[test]
fn durable_only_reader_stops_at_checkpoint() {
    for mapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut writer, 10);
        let mut reader = open(dir.path(), mapped);
        assert!(reader.try_next().unwrap().is_none());
        assert_eq!(reader.position(), 0);
        writer.checkpoint().unwrap();
        expect_range(&mut reader, 0..10);
        append_n(&mut writer, 5);
        for _ in 0..3 {
            assert!(reader.try_next().unwrap().is_none());
            assert_eq!(reader.position(), 10);
        }
        writer.checkpoint().unwrap();
        expect_range(&mut reader, 10..15);
    }
}

#[test]
fn durable_only_rolls_and_seeks() {
    for mapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut writer, 1_000);
        writer.checkpoint().unwrap();
        append_n(&mut writer, 1_000);
        assert!(segments(dir.path()).len() >= 3);
        let mut reader = open(dir.path(), mapped);
        expect_range(&mut reader, 0..1_000);
        reader.seek(900).unwrap();
        expect_range(&mut reader, 900..1_000);
        // Seek within the checkpoint's segment, skipping records all the way to the boundary.
        reader.seek(1_001).unwrap();
        assert!(reader.try_next().unwrap().is_none());
        assert_eq!(reader.position(), 1_001);
        // Seek into a later, wholly uncheckpointed segment.
        reader.seek(1_900).unwrap();
        assert!(reader.try_next().unwrap().is_none());
        writer.checkpoint().unwrap();
        expect_range(&mut reader, 1_900..2_000);
        reader.seek(1_000).unwrap();
        expect_range(&mut reader, 1_000..2_000);
    }
}

#[test]
fn next_timeout_waits_for_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 1);
    let mut reader = open(dir.path(), false);
    assert!(
        reader
            .next_timeout(Some(Duration::from_millis(10)))
            .unwrap()
            .is_none()
    );
    let checkpoint = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        writer.checkpoint().unwrap();
    });
    let record = reader
        .next_timeout(Some(Duration::from_secs(5)))
        .unwrap()
        .unwrap();
    assert_eq!(record.seq, 0);
    checkpoint.join().unwrap();
    assert!(reader.try_next().unwrap().is_none());
}

#[test]
fn durable_only_reader_process_stops_at_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let helper = env!("CARGO_BIN_EXE_tt-durable-only");
    let writer = Command::new(helper)
        .arg("write")
        .arg(dir.path())
        .args(["1000", "1000"])
        .output()
        .unwrap();
    assert!(writer.status.success(), "{writer:?}");
    assert_eq!(read_all(dir.path()).len(), 2_000);
    let reader = Command::new(helper)
        .arg("read")
        .arg(dir.path())
        .arg("1000")
        .output()
        .unwrap();
    assert!(reader.status.success(), "{reader:?}");
    assert_eq!(String::from_utf8(reader.stdout).unwrap(), "1000\n");
}

#[test]
fn checkpoint_boundary_at_seal_preserves_cursor() {
    for mapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        let payload = vec![0; 20_000];
        let record = tickertape::NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: &payload,
        };
        for _ in 0..3 {
            writer.append(&record).unwrap();
        }
        writer.checkpoint().unwrap();
        writer.append(&record).unwrap(); // Writes a seal at exactly D, then rolls.
        assert_eq!(segments(dir.path()).len(), 2);
        let mut reader = open(dir.path(), mapped);
        for seq in 0..3 {
            assert_eq!(reader.next_owned().unwrap().unwrap().seq, seq);
        }
        assert!(reader.try_next().unwrap().is_none());
        assert_eq!(reader.position(), 3);
        writer.checkpoint().unwrap();
        assert_eq!(reader.next_owned().unwrap().unwrap().seq, 3);
        assert!(reader.try_next().unwrap().is_none());
    }
}

// A checkpoint marker speaks about the journal's current files (ADR 0034). A durable-only reader applies a marker that
// moves past its cursor only after confirming, after reading it, that its segment's path still names the file it has
// open. The tests below repair a segment the reader holds (recovery replaces it with a new file; the reader keeps
// valid bytes where recovery found corruption) at each point relative to the marker read and the identity check, and
// check that the reader never returns a record that a reader of the repaired journal doesn't also return.

/// Appends `n` records of a history tagged `tag` (as schema id; `append_n` only uses schema ids 0..7), each with a
/// payload that names the tag and its sequence number, so records of different histories differ.
fn append_history(writer: &mut Writer, n: u64, tag: u32) {
    for _ in 0..n {
        let payload = format!("history {tag} record {}", writer.next_seq());
        writer
            .append(&NewRecord {
                schema_id: tag,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: payload.as_bytes(),
            })
            .unwrap();
    }
}

/// A power loss as recovery sees it: record `corrupt` is damaged in the file the next writer reads (open readers
/// keep the original). The next writer keeps the records before it, appends `checkpointed` records of history `tag`
/// and checkpoints, appends `tail` more, and stops.
fn repair(dir: &std::path::Path, corrupt: u64, checkpointed: u64, tail: u64, tag: u32) {
    corrupt_by_replacement(dir, corrupt);
    let mut writer = reopen_writer(dir, small_config());
    assert_eq!(
        writer.next_seq(),
        corrupt,
        "recovery keeps the records before the corrupt one"
    );
    append_history(&mut writer, checkpointed, tag);
    writer.checkpoint().unwrap();
    append_history(&mut writer, tail, tag);
}

/// Runs `action` the next time this thread's reader reaches `point`.
fn at(point: Point, action: impl FnOnce() + 'static) {
    tickertape::fault::clear();
    tickertape::fault::run_at(point, 0, action);
}

/// Reads until the reader's position reaches `end`, seeking past `Error::Discarded`. Returns the records and the
/// positions at which the reader reported `Discarded`. Waits up to a second for each record, so that a reader that
/// notices a replacement only after waiting at an empty slot (100 ms) also gets there.
fn drain(reader: &mut Reader, end: u64) -> (Vec<OwnedRecord>, Vec<u64>) {
    let (mut records, mut discarded) = (Vec::new(), Vec::new());
    while reader.position() < end {
        match reader
            .next_timeout(Some(Duration::from_secs(1)))
            .map(|r| r.map(|r| r.to_owned()))
        {
            Ok(Some(record)) => records.push(record),
            Ok(None) => panic!("no record at {} (end {end})", reader.position()),
            Err(Error::Discarded { seq }) => {
                assert_eq!(seq, reader.position());
                assert!(discarded.len() < 10, "discarded again and again at {seq}");
                discarded.push(seq);
                reader.seek(seq).unwrap();
            }
            Err(e) => panic!("{e}"),
        }
    }
    assert!(
        reader.try_next().unwrap().is_none(),
        "returned a record past the checkpoint"
    );
    (records, discarded)
}

/// Every record must be the one a reader of the repaired journal returns at its sequence number, and the records
/// must be `range`, in order.
fn assert_repaired_history(
    dir: &std::path::Path,
    records: &[OwnedRecord],
    range: std::ops::Range<u64>,
) {
    let journal = read_all(dir);
    let first = journal.first().map_or(0, |r| r.seq);
    for record in records {
        let current = &journal[(record.seq - first) as usize];
        assert_eq!(
            record, current,
            "durable-only reader returned a record from a discarded history: {}",
            record.seq
        );
    }
    let seqs: Vec<u64> = records.iter().map(|r| r.seq).collect();
    assert_eq!(seqs, range.collect::<Vec<_>>());
}

/// Found by the soak test (seed 1795058278276883001): the reader waits at the checkpoint inside segment X, recovery
/// replaces X, and the new writer appends a different history and checkpoints. The reader read the new marker and
/// kept reading the old X, returning removed records as durable. Now it returns `Error::Discarded` and continues in
/// the repaired history after a seek.
#[test]
fn durable_only_reader_never_returns_records_removed_by_a_repair() {
    for mapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut writer, 10);
        writer.checkpoint().unwrap();
        append_n(&mut writer, 10); // 10..20, not checkpointed
        drop(writer);
        let mut reader = open(dir.path(), mapped);
        expect_range(&mut reader, 0..10);

        repair(dir.path(), 15, 10, 0, 99); // 15..25 of a new history, checkpointed
        let (records, discarded) = drain(&mut reader, 25);
        assert_repaired_history(dir.path(), &records, 10..25);
        assert_eq!(discarded, [10], "mapped {mapped}");
    }
}

/// The repair and the new writer's checkpoint land just before the reader reads the marker: where they would land
/// after an identity check made before the read (the order ADR 0034 rejects, because that check would pass).
#[test]
fn repair_before_the_marker_read_is_discarded() {
    for mapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut writer, 10);
        writer.checkpoint().unwrap();
        append_n(&mut writer, 10);
        drop(writer);
        let mut reader = open(dir.path(), mapped);
        expect_range(&mut reader, 0..10);

        let path = dir.path().to_path_buf();
        at(Point::ReaderCheckpoint, move || {
            repair(&path, 15, 10, 0, 99)
        });
        match reader.next_owned() {
            Err(Error::Discarded { seq: 10 }) => {}
            other => panic!(
                "mapped {mapped}: {:?}",
                other.map(|r| r.map(|r| (r.seq, r.schema_id)))
            ),
        }
        reader.seek(10).unwrap();
        let (records, discarded) = drain(&mut reader, 25);
        assert_repaired_history(dir.path(), &records, 10..25);
        assert!(discarded.is_empty());
    }
}

/// The reader reads a marker of the old writer (covering records the repair keeps), then the repair and the new
/// writer's checkpoint land before its identity check. The check fails, so the reader doesn't apply even the old
/// marker: `Error::Discarded`, then the repaired history after a seek.
#[test]
fn repair_between_the_marker_read_and_the_identity_check_is_discarded() {
    for mapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut writer, 10);
        writer.checkpoint().unwrap();
        let mut reader = open(dir.path(), mapped);
        expect_range(&mut reader, 0..10);
        append_n(&mut writer, 10);
        writer.checkpoint().unwrap(); // covers 10..20
        append_n(&mut writer, 10); // 20..30, not checkpointed
        drop(writer);

        let path = dir.path().to_path_buf();
        at(Point::ReaderCheckpointIdentity, move || {
            repair(&path, 25, 10, 0, 99) // 25..35 of a new history
        });
        let (records, discarded) = drain(&mut reader, 35);
        assert_repaired_history(dir.path(), &records, 10..35);
        assert_eq!(discarded, [10], "mapped {mapped}");
    }
}

/// The identity check passes, then the repair lands. The marker the reader applied was the old writer's, and the
/// repair kept every record it covers, so those records are still right. At the new writer's marker the check fails.
#[test]
fn repair_after_the_identity_check_is_seen_at_the_next_marker() {
    for mapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut writer, 10);
        writer.checkpoint().unwrap();
        let mut reader = open(dir.path(), mapped);
        expect_range(&mut reader, 0..10);
        append_n(&mut writer, 10);
        writer.checkpoint().unwrap();
        append_n(&mut writer, 10);
        drop(writer);

        let mut records = vec![reader.next_owned().unwrap().unwrap()]; // applies the marker at 20
        repair(dir.path(), 25, 10, 0, 99);
        let (rest, discarded) = drain(&mut reader, 35);
        records.extend(rest);
        assert_repaired_history(dir.path(), &records, 10..35);
        assert_eq!(discarded, [20], "mapped {mapped}");
    }
}

/// Two repairs between the marker read and the identity check: the path names neither the reader's file nor the
/// first repair's. And a reader that holds the first repair's file when the second repair lands. A file's identity
/// can't come back (the reader's descriptor keeps its inode allocated), so any repair since the open is seen.
#[test]
fn second_repair_before_the_identity_check_is_discarded() {
    for mapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut writer, 10);
        writer.checkpoint().unwrap();
        let mut reader = open(dir.path(), mapped);
        expect_range(&mut reader, 0..10);
        append_n(&mut writer, 10);
        writer.checkpoint().unwrap();
        append_n(&mut writer, 10);
        drop(writer);

        // The reader holds the original file: two repairs land after it reads the old writer's marker.
        let path = dir.path().to_path_buf();
        at(Point::ReaderCheckpointIdentity, move || {
            repair(&path, 25, 10, 10, 99); // 25..35 checkpointed, 35..45 not
            repair(&path, 40, 10, 0, 98); // 40..50 of a third history
        });
        let (records, discarded) = drain(&mut reader, 50);
        assert_repaired_history(dir.path(), &records, 10..50);
        assert_eq!(discarded, [10], "mapped {mapped}");

        // The reader holds the third history's file; the writer checkpoints, then a repair lands after the reader
        // reads that marker.
        let mut writer = reopen_writer(dir.path(), small_config());
        append_history(&mut writer, 10, 98);
        writer.checkpoint().unwrap(); // covers 50..60
        append_history(&mut writer, 10, 98); // 60..70, not checkpointed
        drop(writer);
        let path = dir.path().to_path_buf();
        at(Point::ReaderCheckpointIdentity, move || {
            repair(&path, 65, 10, 0, 97)
        });
        let (records, discarded) = drain(&mut reader, 75);
        assert_repaired_history(dir.path(), &records, 50..75);
        assert_eq!(discarded, [50], "mapped {mapped}");
    }
}

/// Storage damage zeroes the commit word of record `damaged`, which no checkpoint covers, in the segment file itself
/// (C11). Recovery takes it for the end of the committed records and zeroes the rest of the segment in place: the
/// file keeps its identity, so no identity check can see the repair. The next writer appends `checkpointed` records of
/// history `tag` into the same file, from `damaged` on, and checkpoints.
fn repair_in_place(dir: &std::path::Path, damaged: u64, checkpointed: u64, tag: u32) {
    let identity = |path: &std::path::Path| {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).unwrap();
        (meta.dev(), meta.ino())
    };
    let path = segments(dir).pop().unwrap().1;
    let before = identity(&path);
    zero_commit_word(dir, damaged);
    let mut writer = reopen_writer(dir, small_config());
    assert_eq!(
        writer.next_seq(),
        damaged,
        "recovery keeps the records before the damaged one"
    );
    assert!(
        writer
            .recovery_report()
            .actions
            .iter()
            .any(|a| matches!(a, RecoveryAction::ZeroedUncommitted { .. })),
        "{:?}",
        writer.recovery_report().actions
    );
    append_history(&mut writer, checkpointed, tag);
    writer.checkpoint().unwrap();
    assert_eq!(identity(&path), before, "repaired in place");
}

/// A safe durable-only reader validates records in its read buffer, which can hold bytes it read past the checkpoint
/// before the repair. Records 10..20 are not checkpointed and the reader has buffered them; recovery removes records
/// from `damaged` on in place and the new writer checkpoints a different history there (whose records have other
/// lengths, so the old record boundaries fall inside new records). The reader must read every record the new marker
/// covers from the current file: the kept records, then the new history, with no `Error::Discarded` (nothing it
/// returned was removed) and no corruption report. The middle record (15) and the newest record (19) are damaged in
/// turn; the reader waits at the checkpoint (10) or stops inside the coverage of a marker it already applied (13 of
/// 0..15).
#[test]
fn durable_only_reader_rereads_the_tail_after_an_in_place_repair() {
    for (checkpoint, stop) in [(10, 10), (15, 13)] {
        for damaged in [15, 19] {
            if damaged < checkpoint {
                continue;
            }
            for mapped in [false, true] {
                let case = format!(
                    "checkpoint {checkpoint}, stop {stop}, damaged {damaged}, mapped {mapped}"
                );
                let dir = tempfile::tempdir().unwrap();
                let mut writer = Writer::open(dir.path(), small_config()).unwrap();
                append_n(&mut writer, checkpoint);
                writer.checkpoint().unwrap();
                append_n(&mut writer, 20 - checkpoint); // up to 20, not checkpointed
                drop(writer);
                let mut reader = open(dir.path(), mapped);
                for seq in 0..stop {
                    let record = reader.next_owned().unwrap().expect("checkpointed record");
                    assert_eq!(record.seq, seq, "{case}");
                    check_record(&record);
                }

                repair_in_place(dir.path(), damaged, 10, 99);
                let end = damaged + 10;
                let (records, discarded) = drain(&mut reader, end);
                assert_repaired_history(dir.path(), &records, stop..end);
                assert!(discarded.is_empty(), "{case}: {discarded:?}");
                assert!(
                    records[(damaged - stop) as usize..]
                        .iter()
                        .all(|r| r.schema_id == 99),
                    "{case}"
                );

                // The cursor is on a record boundary of the new history: the reader follows it on.
                let mut writer = reopen_writer(dir.path(), small_config());
                append_history(&mut writer, 10, 99);
                writer.checkpoint().unwrap();
                let (records, discarded) = drain(&mut reader, end + 10);
                assert_repaired_history(dir.path(), &records, end..end + 10);
                assert!(discarded.is_empty(), "{case}: {discarded:?}");
            }
        }
    }
}

/// A durable-only reader whose segment retention removed learns of it when the checkpoint moves past its cursor
/// (before ADR 0034, at the removed segment's end). A visible reader at the same position still reads on to the end
/// of the file it holds (ADR 0016), and so does a durable-only reader whose cached coverage already extends past the
/// removed segment.
#[test]
fn durable_only_reader_overtaken_by_retention_is_discarded_when_the_checkpoint_moves() {
    for mapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut writer, 10);
        writer.checkpoint().unwrap();
        let mut durable = open(dir.path(), mapped);
        expect_range(&mut durable, 0..10);
        let mut visible = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
        for seq in 0..10 {
            assert_eq!(visible.next_owned().unwrap().unwrap().seq, seq);
        }
        append_n(&mut writer, 2_000);
        writer.checkpoint().unwrap();
        // A second durable-only reader that has already applied the marker at 2010.
        let mut covered = open(dir.path(), mapped);
        assert_eq!(covered.next_owned().unwrap().unwrap().seq, 0);
        let second = segments(dir.path())[1].0;
        let plan = retention::plan(dir.path(), &Policy::default().max_segments(2)).unwrap();
        retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
        let first = segments(dir.path())[0].0;
        assert!(first > second);

        match durable.next_owned() {
            Err(Error::Discarded { seq: 10 }) => {}
            other => panic!("mapped {mapped}: {other:?}"),
        }
        match durable.seek(10) {
            Err(Error::SeqUnavailable {
                requested: 10,
                first: f,
            }) => assert_eq!(f, first),
            other => panic!("mapped {mapped}: {other:?}"),
        }
        durable.seek(first).unwrap();
        assert_eq!(durable.next_owned().unwrap().unwrap().seq, first);

        for reader in [&mut visible, &mut covered] {
            let mut expected = reader.position();
            loop {
                match reader.next_owned() {
                    Ok(Some(record)) => {
                        assert_eq!(record.seq, expected);
                        check_record(&record);
                        expected += 1;
                    }
                    Err(Error::Discarded { seq }) => {
                        assert_eq!(seq, second, "discarded at the removed segment's end");
                        break;
                    }
                    other => panic!("mapped {mapped}: {other:?}"),
                }
            }
        }
    }
}

/// A visible reader holding a segment that recovery replaced may return the old file's records after the repair:
/// they were committed (visible) before the crash, which is all a visible reader promises, and are like records lost
/// to a power loss that it had already seen (C8). It reports `Error::Discarded` within the identity-check interval of
/// reaching the old file's end, and never continues into the repaired history without it. Passes before and after
/// ADR 0034, which doesn't change visible readers.
#[test]
fn visible_reader_after_a_repair_is_discarded_at_the_old_end() {
    for mapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut writer, 10);
        writer.checkpoint().unwrap();
        append_n(&mut writer, 10);
        drop(writer);
        let config = ReaderConfig::default().notify(false);
        let mut reader = if mapped {
            // SAFETY: only the tickertape writer modifies segment files in this test.
            unsafe { Reader::open_mapped(dir.path(), config) }.unwrap()
        } else {
            Reader::open(dir.path(), config).unwrap()
        };
        for seq in 0..10 {
            assert_eq!(reader.next_owned().unwrap().unwrap().seq, seq);
        }

        repair(dir.path(), 15, 10, 0, 99);
        for seq in 10..20 {
            let record = reader.next_owned().unwrap().unwrap();
            assert_eq!(record.seq, seq);
            check_record(&record); // the old history, including the removed records 15..20
        }
        let started = std::time::Instant::now();
        match reader.next_timeout(Some(Duration::from_secs(5))) {
            Err(Error::Discarded { seq: 20 }) => {}
            other => panic!("mapped {mapped}: {:?}", other.map(|r| r.map(|r| r.seq))),
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        reader.seek(20).unwrap();
        let (records, discarded) = drain(&mut reader, 25);
        assert_repaired_history(dir.path(), &records, 20..25);
        assert!(discarded.is_empty());
        assert!(records.iter().all(|r| r.schema_id == 99));
    }
}
