//! Crash contract C13: readers expose only the checkpointed prefix.

use std::process::Command;
use std::time::Duration;

use tickertape::fault::Point;
use tickertape::retention::{self, Action, Policy};
use tickertape::{
    Error, NewRecord, OwnedRecord, Reader, ReaderConfig, RecoveryAction, Writer, format,
};
use tickertape_tests::{
    append_n, check_record, corrupt_by_replacement, locate_record, open_raw, read_all, segments,
    small_config, zero_commit_word, zero_commit_words_from,
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
    let mut writer = Writer::open(dir, small_config()).unwrap();
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
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
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

/// Storage damage zeroes the commit words of record `damaged`, which no checkpoint covers, and of every record after
/// it, in the segment file itself (C11): damage recovery can't tell from a torn append (ADR 0035; a single zeroed
/// word with committed records after it would make it replace the file instead). Recovery takes it for the end of
/// the committed records and zeroes the rest of the segment in place: the file keeps its identity, so no identity
/// check can see the repair. The next writer appends `checkpointed` records of
/// history `tag` into the same file, from `damaged` on, and checkpoints.
fn repair_in_place(dir: &std::path::Path, damaged: u64, checkpointed: u64, tag: u32) {
    let identity = |path: &std::path::Path| {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).unwrap();
        (meta.dev(), meta.ino())
    };
    let path = segments(dir).pop().unwrap().1;
    let before = identity(&path);
    zero_commit_words_from(dir, damaged);
    let mut writer = Writer::open(dir, small_config()).unwrap();
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
/// returned was removed) and no corruption report. The records from the middle one (15) on, and the newest record
/// (19) alone, are damaged in turn; the reader waits at the checkpoint (10) or stops inside the coverage of a marker it already applied (13 of
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
                let mut writer = Writer::open(dir.path(), small_config()).unwrap();
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

/// The checkpoint boundary at a segment's end, with an empty successor (found by review). A durable-only reader has
/// read every record of segment A (the marker covers them all); the writer rolls to B without committing a record
/// there (a caught panic in `append_with`'s `fill` after the roll), checkpoints (the marker names B, at the reader's
/// cursor), and retention removes A. The marker never moves past the cursor while the writer is idle, so the reader
/// never reached an identity check: before the fix, both durable-only readers returned `Ok(None)` on every call, where
/// a visible reader returns `Error::Discarded`. Now a durable-only reader waiting at the checkpoint with a marker
/// naming a later segment does what a visible reader does at the seal: it follows the seal to B if A is in place and
/// returns `Discarded` at its cursor if A is gone (ADR 0034 → Readers overtaken by retention).
#[test]
fn durable_only_reader_at_a_removed_segment_end_with_an_empty_successor_is_discarded() {
    for mapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        let payload = vec![0; 20_000];
        let record = NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: &payload,
        };
        for _ in 0..3 {
            writer.append(&record).unwrap(); // Three fill the segment.
        }
        writer.checkpoint().unwrap();
        let mut rolled = open(dir.path(), mapped);
        let mut stale = open(dir.path(), mapped);
        let mut visible = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
        for reader in [&mut rolled, &mut stale, &mut visible] {
            for seq in 0..3 {
                assert_eq!(reader.next_owned().unwrap().unwrap().seq, seq);
            }
            assert!(reader.try_next().unwrap().is_none());
        }

        // The fourth record doesn't fit: the writer rolls to segment 3, then the fill panics, so nothing is committed
        // there.
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = writer.append_with(&record, payload.len(), |_| panic!("fill failed"));
        }));
        assert!(panicked.is_err());
        assert_eq!(writer.next_seq(), 3);
        assert_eq!(
            segments(dir.path()).iter().map(|s| s.0).collect::<Vec<_>>(),
            [0, 3]
        );
        writer.checkpoint().unwrap();
        let marker = tickertape::inspect::read_checkpoint(dir.path())
            .unwrap()
            .unwrap();
        assert_eq!((marker.segment_base_seq, marker.next_seq), (3, 3));

        // With segment 0 still in place, the reader moves to segment 3 and keeps it after the removal.
        assert!(rolled.try_next().unwrap().is_none());
        let plan = retention::plan(dir.path(), &Policy::default().max_segments(1)).unwrap();
        retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
        assert_eq!(
            segments(dir.path()).iter().map(|s| s.0).collect::<Vec<_>>(),
            [3]
        );
        assert!(rolled.try_next().unwrap().is_none());

        // A reader that hadn't looked since the roll finds segment 0 gone, as the visible reader does.
        for reader in [&mut stale, &mut visible] {
            match reader.try_next() {
                Err(Error::Discarded { seq: 3 }) => {}
                other => panic!("mapped {mapped}: {:?}", other.map(|r| r.map(|r| r.seq))),
            }
            reader.seek(3).unwrap();
        }

        writer.append(&record).unwrap();
        assert!(stale.try_next().unwrap().is_none(), "not checkpointed yet");
        writer.checkpoint().unwrap();
        for reader in [&mut rolled, &mut stale, &mut visible] {
            let record = reader.next_owned().unwrap().unwrap();
            assert_eq!(record.seq, 3);
            assert!(reader.try_next().unwrap().is_none());
        }
    }
}

/// The same boundary when the safe reader's buffer holds a record recovery removed in place (found by review). A
/// short record 3, never checkpointed, still fits in segment 0 after three large checkpointed ones; the reader buffers
/// it while reading 0..3. Storage damage empties its commit word, recovery zeroes it in place (segment 0 keeps its
/// identity), and the new writer rolls to segment 3 for a large record whose fill panics, sealing segment 0 where
/// record 3 was, and checkpoints (the marker names segment 3, at the reader's cursor). The reader must see the seal now
/// on disk at its cursor and move to segment 3. Before the fix, the safe reader probed the slot from its buffer, saw
/// the removed record 3 there on every poll, never moved, and reported `Error::Discarded` once retention removed
/// segment 0 although it had polled while segment 0 was current. The mapped reader (which reads the mapping) is the
/// control.
#[test]
fn durable_only_reader_sees_a_seal_written_over_a_buffered_record() {
    for mapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        let payload = vec![0; 20_000];
        let record = NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: &payload,
        };
        for _ in 0..3 {
            writer.append(&record).unwrap();
        }
        writer.checkpoint().unwrap();
        let short = NewRecord {
            payload: &payload[..100],
            ..record
        };
        writer.append(&short).unwrap(); // Record 3, not checkpointed, in segment 0.
        drop(writer);
        assert_eq!(segments(dir.path()).len(), 1);
        let mut reader = open(dir.path(), mapped);
        for seq in 0..3 {
            assert_eq!(reader.next_owned().unwrap().unwrap().seq, seq);
        }
        assert!(reader.try_next().unwrap().is_none());

        zero_commit_word(dir.path(), 3);
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        assert_eq!(writer.next_seq(), 3);
        assert!(
            writer
                .recovery_report()
                .actions
                .iter()
                .any(|a| matches!(a, RecoveryAction::ZeroedUncommitted { .. }))
        );
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = writer.append_with(&record, payload.len(), |_| panic!("fill failed"));
        }));
        assert!(panicked.is_err());
        assert_eq!(
            segments(dir.path()).iter().map(|s| s.0).collect::<Vec<_>>(),
            [0, 3]
        );
        writer.checkpoint().unwrap();
        let marker = tickertape::inspect::read_checkpoint(dir.path())
            .unwrap()
            .unwrap();
        assert_eq!((marker.segment_base_seq, marker.next_seq), (3, 3));

        // Segment 0 is still in place: the reader follows its seal to segment 3 and keeps it after the removal.
        assert!(reader.try_next().unwrap().is_none());
        let plan = retention::plan(dir.path(), &Policy::default().max_segments(1)).unwrap();
        retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
        assert_eq!(
            segments(dir.path()).iter().map(|s| s.0).collect::<Vec<_>>(),
            [3]
        );
        // Past the identity-check interval, so a reader still holding segment 0 would check it now.
        std::thread::sleep(Duration::from_millis(150));
        match reader.try_next() {
            Ok(None) => {}
            other => panic!("mapped {mapped}: {:?}", other.map(|r| r.map(|r| r.seq))),
        }

        writer.append(&record).unwrap();
        writer.checkpoint().unwrap();
        let read = reader.next_owned().unwrap().unwrap();
        assert_eq!((read.seq, read.payload.len()), (3, payload.len()));
        assert!(reader.try_next().unwrap().is_none());
    }
}

/// A `stat` that fails other than with "not found" (here `EACCES`) at the identity check after reading a marker that
/// moved past the cursor is an I/O error, and the reader doesn't apply that marker: the next call reads the marker
/// again and checks again. Had it applied the marker, the replacement below would go unnoticed and the reader would
/// return record 10 from the old file.
#[test]
fn identity_check_that_fails_leaves_the_marker_unapplied() {
    use std::os::unix::fs::PermissionsExt;
    for mapped in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::open(dir.path(), small_config()).unwrap();
        append_n(&mut writer, 10);
        writer.checkpoint().unwrap();
        let mut reader = open(dir.path(), mapped);
        expect_range(&mut reader, 0..10);
        append_n(&mut writer, 10);
        writer.checkpoint().unwrap();

        let path = dir.path().to_path_buf();
        let denied = std::rc::Rc::new(std::cell::Cell::new(false));
        let d = denied.clone();
        at(Point::ReaderCheckpointIdentity, move || {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
            d.set(
                std::fs::metadata(path.join("probe"))
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied),
            );
        });
        let result = reader.next_owned();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        tickertape::fault::clear();
        if !denied.get() {
            eprintln!(
                "identity_check_that_fails_leaves_the_marker_unapplied: permissions have no effect (root?); skipped"
            );
            return;
        }
        match result {
            Err(Error::Io { ref source, .. })
                if source.kind() == std::io::ErrorKind::PermissionDenied => {}
            other => panic!("mapped {mapped}: {:?}", other.map(|r| r.map(|r| r.seq))),
        }
        assert_eq!(reader.position(), 10);

        // A copy renamed over the segment: same records, another file.
        let segment = segments(dir.path())[0].1.clone();
        let copy = dir.path().join("copy");
        std::fs::copy(&segment, &copy).unwrap();
        std::fs::rename(&copy, &segment).unwrap();
        match reader.next_owned() {
            Err(Error::Discarded { seq: 10 }) => {}
            other => panic!("mapped {mapped}: {:?}", other.map(|r| r.map(|r| r.seq))),
        }
        reader.seek(10).unwrap();
        expect_range(&mut reader, 10..20);
    }
}

/// `seek` to a position past the start of a segment (`skip_until` above the base: the reader replays the segment from
/// its start, skipping records before the target), then a repair of that segment before the next read. The reader
/// had applied the marker at 10; the targets lie before it (5) and after it (12). A replacement gives
/// `Error::Discarded` at the reader's position once a new marker moves past the cursor; an in-place repair is read
/// through. Either way every record returned is the repaired journal's.
#[test]
fn seek_inside_a_segment_then_a_repair_before_the_next_read() {
    for in_place in [false, true] {
        for target in [5, 12] {
            for mapped in [false, true] {
                let case = format!("in place {in_place}, target {target}, mapped {mapped}");
                let dir = tempfile::tempdir().unwrap();
                let mut writer = Writer::open(dir.path(), small_config()).unwrap();
                append_n(&mut writer, 10);
                writer.checkpoint().unwrap();
                append_n(&mut writer, 10); // 10..20, not checkpointed
                drop(writer);
                let mut reader = open(dir.path(), mapped);
                expect_range(&mut reader, 0..10);
                reader.seek(target).unwrap();
                assert_eq!(reader.position(), target);

                if in_place {
                    repair_in_place(dir.path(), 15, 10, 99); // 15..25 of a new history, checkpointed
                } else {
                    repair(dir.path(), 15, 10, 0, 99);
                }
                let (records, discarded) = drain(&mut reader, 25);
                assert_repaired_history(dir.path(), &records, target..25);
                let expected: &[u64] = match (in_place, target) {
                    (true, _) => &[],
                    (false, 5) => &[10],
                    (false, _) => &[12],
                };
                assert_eq!(discarded, expected, "{case}");
                assert!(
                    records
                        .iter()
                        .filter(|r| r.seq >= 15)
                        .all(|r| r.schema_id == 99),
                    "{case}"
                );
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

/// A journal of `small_config` segments whose segment 0 is sealed: records 0..100 checkpointed, then records up to 10
/// past the roll, and the writer dropped. Returns the successor's base and the seal's offset in segment 0.
fn sealed_journal(dir: &std::path::Path) -> (u64, u64) {
    let mut writer = Writer::open(dir, small_config()).unwrap();
    append_n(&mut writer, 100);
    writer.checkpoint().unwrap();
    while segments(dir).len() < 2 {
        append_n(&mut writer, 1);
    }
    append_n(&mut writer, 10);
    drop(writer);
    let successor = segments(dir)[1].0;
    let (path, offset) = locate_record(dir, successor - 1);
    let image = std::fs::read(path).unwrap();
    match format::read_slot(&image[..], offset, successor - 1) {
        Ok(format::Slot::Record { next_offset, .. }) => (successor, next_offset),
        other => panic!("record {}: {other:?}", successor - 1),
    }
}

/// Storage damage zeroes segment 0's seal marker and the successor's header, so the successor no longer opens. The next
/// writer's recovery can't tell the seal's leftover CRC word from a crash between publishing a successor and sealing
/// with its header since damaged (ADR 0035 → Damage around a seal): it zeroes that word **in place**, so segment 0 keeps
/// its identity and ends at the seal's place, and quarantines the successor as disconnected. The new writer begins
/// there. A reader can't see this repair, and the journal it leaves is healthy.
fn remove_seal_in_place(dir: &std::path::Path, successor: u64, seal: u64) -> Writer {
    use std::os::unix::fs::{FileExt, MetadataExt};
    let identity = |path: &std::path::Path| {
        let meta = std::fs::metadata(path).unwrap();
        (meta.dev(), meta.ino())
    };
    let segs = segments(dir);
    let before = identity(&segs[0].1);
    open_raw(&segs[0].1).write_all_at(&[0; 4], seal).unwrap();
    open_raw(&segs[1].1).write_all_at(b"X", 0).unwrap(); // damages the magic
    let writer = Writer::open(dir, small_config()).unwrap();
    let actions = &writer.recovery_report().actions;
    assert!(
        matches!(
            actions.as_slice(),
            [
                RecoveryAction::ZeroedUncommitted { offset, len: 4, .. },
                RecoveryAction::QuarantinedSegment { .. },
            ] if *offset == seal + 4
        ),
        "{actions:?}"
    );
    assert_eq!(writer.next_seq(), successor);
    assert_eq!(segments(dir).len(), 1);
    assert_eq!(identity(&segs[0].1), before, "repaired in place");
    writer
}

/// Found by `reader_retention_stress::durable_readers_under_repairs_long` (CI, arm64 Linux, seed
/// 1795100883310377747): a visible safe reader reported `Corrupt { BrokenChain }` on a healthy journal. It had read
/// segment 0 into its buffer up to the seal; recovery then removed the seal in place and quarantined the successor,
/// and the reader followed the seal it still held, found no successor while its segment was in place, and reported a
/// broken chain on every call until the new writer happened to roll to the same base again (forever, if it never
/// does). Before ADR 0035 one zeroed commit word in a sealed segment was enough; now the only in-place repair that
/// removes a seal is the one in [`remove_seal_in_place`]. A zero-copy reader reads the current bytes, but can load
/// the seal just before the repair.
///
/// The seal it read is gone from the file, so it isn't corruption: the reader reads the slot again and waits there
/// (`Ok(None)`), then continues in the new history, which kept the file's identity, without `Error::Discarded` (C11).
/// Three moments: the repair before the reader reaches the seal (the safe reader has it buffered; the zero-copy
/// reader sees the empty slot), after it read the seal and before it opens the successor, and the same with the new
/// writer rolling again before the reader reads the slot again (the seal is back, and its successor opens).
#[test]
fn visible_reader_does_not_follow_a_seal_an_in_place_repair_removed() {
    use std::cell::RefCell;
    use std::rc::Rc;
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum When {
        BeforeTheSeal,
        AtTheSeal,
        AtTheSealThenRoll,
    }
    for when in [
        When::BeforeTheSeal,
        When::AtTheSeal,
        When::AtTheSealThenRoll,
    ] {
        for mapped in [false, true] {
            let case = format!("{when:?}, mapped {mapped}");
            let dir = tempfile::tempdir().unwrap();
            let (successor, seal) = sealed_journal(dir.path());
            let config = ReaderConfig::default().notify(false);
            let mut reader = if mapped {
                // SAFETY: only the tickertape writer modifies segment files in this test, apart from the damage
                // injected while no writer runs (to bytes the reader has not validated and does not hold).
                unsafe { Reader::open_mapped(dir.path(), config) }.unwrap()
            } else {
                Reader::open(dir.path(), config).unwrap()
            };
            // The safe reader now holds segment 0 in its buffer, seal included.
            for seq in 0..successor {
                let record = reader.next_owned().unwrap().unwrap();
                assert_eq!(record.seq, seq, "{case}");
                check_record(&record);
            }
            let writer = Rc::new(RefCell::new(None));
            let path = dir.path().to_path_buf();
            if when == When::BeforeTheSeal {
                *writer.borrow_mut() = Some(remove_seal_in_place(&path, successor, seal));
            } else {
                let (w, p) = (writer.clone(), path.clone());
                at(Point::ReaderSeal, move || {
                    *w.borrow_mut() = Some(remove_seal_in_place(&p, successor, seal));
                });
            }
            if when == When::AtTheSealThenRoll {
                let w = writer.clone();
                tickertape::fault::run_at(Point::ReaderSealReread, 0, move || {
                    let mut w = w.borrow_mut();
                    let writer = w.as_mut().expect("repaired at the seal");
                    append_history(writer, 1, 99);
                    assert_eq!(
                        segments(&path).len(),
                        2,
                        "the new writer rolled to {successor}"
                    );
                });
            }
            let first = reader.try_next().map(|r| r.map(|r| r.to_owned()));
            tickertape::fault::clear();
            match (when, first) {
                (When::AtTheSealThenRoll, Ok(Some(record))) => {
                    assert_eq!((record.seq, record.schema_id), (successor, 99), "{case}");
                }
                (When::BeforeTheSeal | When::AtTheSeal, Ok(None)) => {
                    assert!(reader.try_next().unwrap().is_none(), "{case}");
                    let mut w = writer.borrow_mut();
                    append_history(w.as_mut().expect("repaired"), 1, 99);
                    let record = reader
                        .next_timeout(Some(Duration::from_secs(5)))
                        .unwrap()
                        .expect("the new history's first record")
                        .to_owned();
                    assert_eq!((record.seq, record.schema_id), (successor, 99), "{case}");
                }
                (_, other) => panic!("{case}: {:?}", other.map(|r| r.map(|r| r.seq))),
            }
            // On in the new history, which the repair kept in segment 0's file: no `Error::Discarded`.
            append_history(writer.borrow_mut().as_mut().unwrap(), 4, 99);
            let (records, discarded) = drain(&mut reader, successor + 5);
            assert!(discarded.is_empty(), "{case}: {discarded:?}");
            assert!(records.iter().all(|r| r.schema_id == 99), "{case}");
            assert_eq!(records.len(), 4, "{case}");
        }
    }
}
