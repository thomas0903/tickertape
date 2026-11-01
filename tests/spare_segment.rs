//! ADR 0022: the next segment is prepared in the background and published at roll (macOS only, ADR 0023).

// The helpers below serve the macOS tests; Linux only runs `no_spare_on_linux`.
#![cfg_attr(not(target_vendor = "apple"), allow(dead_code, unused_imports))]

use std::path::Path;
use std::time::{Duration, Instant};

use std::os::unix::fs::MetadataExt;
use std::sync::Mutex;

use tickertape::fault::{self, Point};
use tickertape::{NewRecord, Reader, ReaderConfig, RecoveryAction, Writer};
use tickertape_tests::small_config;

const SPARE: &str = "next-segment.tmp";

/// Background failure points are process-wide: tests in this file run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    fault::clear();
    guard
}

fn identity(path: &Path) -> (u64, u64) {
    let m = std::fs::metadata(path).unwrap();
    (m.dev(), m.ino())
}

/// The newest segment file.
fn newest_segment(dir: &Path) -> std::path::PathBuf {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "tape"))
        .collect();
    names.sort();
    names.pop().unwrap()
}

/// Appends past the middle of the active segment and waits until its spare exists and is fully prepared.
fn wait_for_ready_spare(w: &mut Writer, dir: &Path) {
    for _ in 0..40 {
        append(w, 1024);
    }
    wait_until_spare_is_ready(dir);
}

/// Waits until the spare exists with its full size and allocated blocks (not sparse).
fn wait_until_spare_is_ready(dir: &Path) {
    let spare = dir.join(SPARE);
    let size = small_config().segment_size;
    wait_for(
        || std::fs::metadata(&spare).is_ok_and(|m| m.len() == size && m.blocks() * 512 >= size),
        "a preallocated spare",
    );
}

fn read_count(dir: &Path) -> u64 {
    let mut r = Reader::open(dir, ReaderConfig::default()).unwrap();
    let mut n = 0;
    while let Some(rec) = r.try_next().unwrap() {
        assert_eq!(rec.seq, n);
        n += 1;
    }
    n
}

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

#[cfg(target_vendor = "apple")]
#[test]
fn spare_is_prepared_at_half_full_and_published_at_roll() {
    let _serial = serial();
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
    wait_until_spare_is_ready(dir.path());
    // The spare is really preallocated (not sparse), and the roll publishes that very file.
    let prepared = identity(&spare);
    fill_until_roll(&mut w, dir.path(), 1);
    assert!(!spare.exists(), "the spare is published by the roll");
    assert_eq!(
        identity(&newest_segment(dir.path())),
        prepared,
        "the roll created a new file instead"
    );
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

#[cfg(target_vendor = "apple")]
#[test]
fn leftover_spare_is_removed_and_reported_at_open() {
    let _serial = serial();
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

#[cfg(target_vendor = "apple")]
#[test]
fn roll_falls_back_when_no_spare_can_be_prepared() {
    let _serial = serial();
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

#[cfg(target_vendor = "apple")]
#[test]
fn failed_spare_preparation_is_cleaned_up_and_the_roll_falls_back() {
    let _serial = serial();
    for (point, errno) in [
        (Point::SparePreallocate, 28 /* ENOSPC */),
        (Point::SpareMap, 12 /* ENOMEM */),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Writer::open(dir.path(), small_config()).unwrap();
        fault::fail_next_background(point, errno);
        for _ in 0..40 {
            append(&mut w, 1024); // past half: the spare is requested and its preparation fails
        }
        fill_until_roll(&mut w, dir.path(), 1);
        assert!(
            !dir.path().join(SPARE).exists(),
            "{point:?}: a failed spare was left behind"
        );
        // The next segment gets a working spare again.
        wait_for_ready_spare(&mut w, dir.path());
        let prepared = identity(&dir.path().join(SPARE));
        fill_until_roll(&mut w, dir.path(), 1);
        assert_eq!(identity(&newest_segment(dir.path())), prepared, "{point:?}");
        let total = w.next_seq();
        drop(w);
        assert_eq!(read_count(dir.path()), total, "{point:?}");
    }
}

#[cfg(target_vendor = "apple")]
#[test]
fn failed_header_write_discards_the_spare_and_creates_the_segment() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    wait_for_ready_spare(&mut w, dir.path());
    // Keep the spare open: otherwise a filesystem may give the replacement the freed inode number (ext4 does).
    let _hold = std::fs::File::open(dir.path().join(SPARE)).unwrap();
    let prepared = identity(&dir.path().join(SPARE));
    fault::fail_next(Point::SpareHeader, 5 /* EIO */);
    fill_until_roll(&mut w, dir.path(), 1);
    fault::clear();
    assert!(
        !dir.path().join(SPARE).exists(),
        "the discarded spare was left behind"
    );
    assert_ne!(
        identity(&newest_segment(dir.path())),
        prepared,
        "the failed spare was published"
    );
    let total = w.next_seq();
    drop(w);
    assert_eq!(read_count(dir.path()), total);
}

#[cfg(not(target_vendor = "apple"))]
#[test]
fn no_spare_on_linux() {
    // ADR 0023: on ext4 the spare occasionally stalled the writer, so other systems roll synchronously.
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    for _ in 0..40 {
        append(&mut w, 1024); // past half of the first segment
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(!dir.path().join(SPARE).exists());
    fill_until_roll(&mut w, dir.path(), 2);
    let total = w.next_seq();
    drop(w);
    assert_eq!(read_count(dir.path()), total);
}
