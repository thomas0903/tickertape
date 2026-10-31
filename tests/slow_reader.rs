//! Crash contract C6: a slow (or stalled) reader never slows the writer, and catches up without gaps.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tickertape::{Reader, ReaderConfig, Writer};
use tickertape_tests::{append_n, check_record, segments, small_config};

const READER: &str = env!("CARGO_BIN_EXE_tt-reader");

#[test]
fn stalled_reader_in_process_catches_up() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 10);
    let mut r = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    for _ in 0..5 {
        r.next_owned().unwrap().unwrap();
    }
    // The reader stalls (holding its segment open) while the writer rolls through many segments.
    append_n(&mut w, 20_000);
    assert!(segments(dir.path()).len() > 20);
    let mut n = 5;
    while let Some(rec) = r.next_owned().unwrap() {
        assert_eq!(rec.seq, n);
        check_record(&rec);
        n += 1;
    }
    assert_eq!(n, 20_010);
}

#[test]
fn slow_reader_process_does_not_block_writer() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal");
    let count = 20_000u64;
    // The reader sleeps 50 µs per record, so it needs at least a second for 20,000 records.
    let reader = Command::new(READER)
        .args([
            journal.to_str().unwrap(),
            &count.to_string(),
            "64",
            "safe",
            "120",
            "50",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(50));
    // Large enough that the writer never rolls: segment creation syncs a file, and on slow CI disks those syncs, not
    // the reader, would dominate the writer's time.
    let mut w = Writer::open(
        &journal,
        tickertape::WriterConfig::default().segment_size(8 * 1024 * 1024),
    )
    .unwrap();
    let started = Instant::now();
    for seq in 0..count {
        let data = tickertape_tests::payload(seq, tickertape_tests::bounded_len(seq, 64));
        w.append(&tickertape::NewRecord {
            schema_id: (seq % 7) as u32,
            venue_ts_ns: seq,
            local_ts_ns: 0,
            payload: &data,
        })
        .unwrap();
    }
    let writer_time = started.elapsed();
    let out = reader.wait_with_output().unwrap();
    let reader_time = started.elapsed();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with(&format!("{count} ")));
    assert!(
        writer_time < reader_time / 2,
        "writer took {writer_time:?}, reader {reader_time:?}: the writer must not wait for the reader"
    );
}
