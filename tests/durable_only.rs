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
