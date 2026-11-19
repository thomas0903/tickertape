//! Crash contract C13: readers expose only the checkpointed prefix.

use std::process::Command;
use std::time::Duration;

use tickertape::{Reader, ReaderConfig, Writer};
use tickertape_tests::{append_n, check_record, read_all, segments, small_config};

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

/// KNOWN FAILURE, found by `tests/soak.rs` (seed 1795058278276883001): a durable-only reader can return records
/// that were never checkpointed and that recovery has removed.
///
/// The reader waits at the checkpoint boundary inside segment X. Recovery finds a corrupt record after the
/// checkpoint, preserves X's tail in `quarantine/` and replaces X with a repaired copy. The new writer appends and
/// checkpoints. The reader refreshes the marker, sees a larger `next_seq`, and keeps reading the file it has open (the
/// replaced X), so it returns the discarded records whose sequence numbers the new marker covers. It only notices
/// the replacement at X's seal (`Error::Discarded`). This needs the reader's open file to hold valid bytes where the
/// file recovery read is corrupt: here a renamed-over copy; in the field, for example, a reader's buffered copy of a
/// page that later failed on the storage device (C11).
///
/// A possible fix (`reader.rs`, `refresh_checkpoint`): when the refreshed `next_seq` is beyond the cursor, check the
/// segment's identity after reading the marker and return `Error::Discarded` if it changed (recovery always replaces
/// files before the new writer's first checkpoint). It adds a check to `docs/format.md` → "What readers do about
/// repairs", and durable-only readers overtaken by retention would then get `Error::Discarded` inside a removed
/// segment rather than at its end, so it is left to the maintainer. When it is fixed, this test fails: remove
/// `should_panic`.
#[test]
#[should_panic(expected = "durable-only reader returned a record from a discarded history")]
fn known_failure_durable_only_reader_returns_discarded_records_after_repair() {
    use tickertape::format::{self, Slot};
    use tickertape::{Error, NewRecord};

    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut writer, 10);
    writer.checkpoint().unwrap();
    append_n(&mut writer, 10); // 10..20, not checkpointed
    drop(writer);

    let mut reader = open(dir.path(), false);
    expect_range(&mut reader, 0..10);
    assert!(
        reader.try_next().unwrap().is_none(),
        "waits at the checkpoint"
    );

    // Record 15 becomes corrupt in the file recovery reads; the reader keeps the file it has open.
    let path = segments(dir.path())[0].1.clone();
    let mut image = std::fs::read(&path).unwrap();
    let (mut offset, mut seq) = (format::SEGMENT_HEADER_LEN as u64, 0);
    while seq < 15 {
        let Ok(Slot::Record { next_offset, .. }) = format::read_slot(&image[..], offset, seq)
        else {
            panic!("record {seq}");
        };
        offset = next_offset;
        seq += 1;
    }
    image[offset as usize + 20] ^= 0xff; // a byte of the record's fields: checksum mismatch
    let tmp = dir.path().join("replacement");
    std::fs::write(&tmp, &image).unwrap();
    std::fs::rename(&tmp, &path).unwrap();

    // Recovery keeps 0..15 and quarantines the rest; the new writer appends a different 15..25 and checkpoints.
    let mut writer = Writer::open(dir.path(), small_config()).unwrap();
    assert_eq!(writer.next_seq(), 15);
    for _ in 0..10 {
        writer
            .append(&NewRecord {
                schema_id: 99,
                venue_ts_ns: 0,
                local_ts_ns: 0,
                payload: b"new history",
            })
            .unwrap();
    }
    writer.checkpoint().unwrap();

    // Records 10..15 are the same in both histories. From 15 on, only the new writer's records (schema 99) are
    // covered by the checkpoint; the reader must return those or `Error::Discarded`, never the old ones.
    let mut returned = 0;
    while returned < 15 {
        match reader.next_owned() {
            Ok(Some(r)) => {
                returned += 1;
                if r.seq >= 15 && r.schema_id != 99 {
                    panic!(
                        "durable-only reader returned a record from a discarded history: {}",
                        r.seq
                    );
                }
            }
            Ok(None) => break,
            Err(Error::Discarded { .. }) => {
                let at = reader.position();
                reader.seek(at).unwrap();
            }
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(reader.position(), 25);
}
