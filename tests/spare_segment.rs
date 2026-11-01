//! ADR 0022: the next segment is prepared in the background and published at roll.

use std::path::Path;
use std::time::{Duration, Instant};

use tickertape::{NewRecord, Reader, ReaderConfig, RecoveryAction, Writer};
use tickertape_tests::small_config;

const SPARE: &str = "next-segment.tmp";

fn append(w: &mut Writer, len: usize) {
    w.append(&NewRecord {
        schema_id: 1,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload: &vec![0x5a; len],
    })
    .unwrap();
}

fn segments(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tape")
        })
        .count()
}

fn wait_for(cond: impl Fn() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Appends 1 KiB records until the writer has rolled `rolls` more times.
fn fill_until_roll(w: &mut Writer, dir: &Path, rolls: usize) {
    let target = segments(dir) + rolls;
    while segments(dir) < target {
        append(w, 1024);
    }
}

#[test]
fn spare_is_prepared_at_half_full_and_published_at_roll() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    let spare = dir.path().join(SPARE);
    assert!(!spare.exists(), "no spare before the segment is half full");
    let half = small_config().segment_size / 2;
    let mut written = 64;
    while written < half + 1024 {
        append(&mut w, 1024);
        written += 1064;
    }
    wait_for(|| spare.exists(), "the spare");
    fill_until_roll(&mut w, dir.path(), 1);
    assert!(!spare.exists(), "the spare is published by the roll");
    // The next segment gets its own spare once it is half full, and every record reads back.
    fill_until_roll(&mut w, dir.path(), 2);
    for _ in 0..40 {
        append(&mut w, 1024); // past half of the new segment
    }
    wait_for(|| spare.exists(), "the next spare");
    let total = w.next_seq();
    drop(w);
    assert!(
        !spare.exists(),
        "dropping the writer removes a prepared spare"
    );
    let mut r = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let mut n = 0;
    while let Some(rec) = r.try_next().unwrap() {
        assert_eq!(rec.seq, n);
        assert_eq!(rec.payload.len(), 1024);
        n += 1;
    }
    assert_eq!(n, total);
}

#[test]
fn leftover_spare_is_removed_and_reported_at_open() {
    let dir = tempfile::tempdir().unwrap();
    drop(Writer::open(dir.path(), small_config()).unwrap());
    std::fs::write(dir.path().join(SPARE), vec![0u8; 4096]).unwrap();
    let w = tickertape_tests::reopen_writer(dir.path(), small_config());
    assert!(!dir.path().join(SPARE).exists());
    assert!(
        w.recovery_report()
            .actions
            .iter()
            .any(|a| matches!(a, RecoveryAction::RemovedTemp { file } if file.ends_with(SPARE))),
        "{:?}",
        w.recovery_report().actions
    );
}

#[test]
fn roll_falls_back_when_no_spare_can_be_prepared() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    // A directory in the spare's place makes every preparation fail.
    std::fs::create_dir(dir.path().join(SPARE)).unwrap();
    fill_until_roll(&mut w, dir.path(), 2);
    let total = w.next_seq();
    drop(w);
    let mut r = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let mut n = 0;
    while r.try_next().unwrap().is_some() {
        n += 1;
    }
    assert_eq!(n, total);
}
