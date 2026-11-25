//! ADR 0022: the next segment is prepared in the background and published at roll (macOS by default;
//! on Linux with `WriterConfig::spare_segment`, ADR 0049).

use std::path::Path;
use std::time::{Duration, Instant};

use std::os::unix::fs::MetadataExt;
use std::sync::Mutex;

use tickertape::fault::{self, Point};
use tickertape::{NewRecord, Reader, ReaderConfig, RecoveryAction, Writer, WriterConfig};
use tickertape_tests::small_config;

const SPARE: &str = "next-segment.tmp";

/// Background failure points are process-wide: tests in this file run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    fault::clear();
    guard
}

/// The spare segment on, which is the default on macOS only.
fn config() -> WriterConfig {
    small_config().spare_segment(true)
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
    wait_until_spare_is_ready(w, dir);
}

/// Waits until the spare is ready for the next roll, with its full size and allocated blocks (not sparse).
fn wait_until_spare_is_ready(w: &Writer, dir: &Path) {
    let spare = dir.join(SPARE);
    let size = small_config().segment_size;
    wait_for(
        || std::fs::metadata(&spare).is_ok_and(|m| m.len() == size && m.blocks() * 512 >= size),
        "a preallocated spare",
    );
    wait_for(|| w.spare_settled(), "the spare's preparation to end");
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

#[test]
fn spare_is_prepared_at_half_full_and_published_at_roll() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), config()).unwrap();
    let spare = dir.path().join(SPARE);
    assert!(!spare.exists(), "no spare before the segment is half full");
    let half = small_config().segment_size / 2;
    let mut written = 64;
    while written < half + 1024 {
        append(&mut w, 1024);
        written += 1064;
    }
    wait_until_spare_is_ready(&w, dir.path());
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
    wait_for(|| w.spare_settled(), "the spare's preparation to end");
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
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    drop(Writer::open(dir.path(), config()).unwrap());
    std::fs::write(dir.path().join(SPARE), vec![0u8; 4096]).unwrap();
    let w = Writer::open(dir.path(), config()).unwrap();
    assert!(!dir.path().join(SPARE).exists());
    assert!(
        w.recovery_report().actions.iter().any(
            |a| matches!(a, RecoveryAction::RemovedTemp { file, .. } if file.ends_with(SPARE))
        ),
        "{:?}",
        w.recovery_report().actions
    );
}

#[test]
fn roll_falls_back_when_no_spare_can_be_prepared() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), config()).unwrap();
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

#[test]
fn failed_spare_preparation_is_cleaned_up_and_the_roll_falls_back() {
    let _serial = serial();
    for (point, errno) in [
        (Point::SparePreallocate, 28 /* ENOSPC */),
        (Point::SpareMap, 12 /* ENOMEM */),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Writer::open(dir.path(), config()).unwrap();
        fault::fail_next_background(point, errno);
        for _ in 0..40 {
            append(&mut w, 1024); // past half: the spare is requested and its preparation fails
        }
        wait_for(|| w.spare_settled(), "the spare's preparation to end");
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

#[test]
fn failed_header_write_discards_the_spare_and_creates_the_segment() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), config()).unwrap();
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

#[test]
fn prefault_off_prepares_no_spare() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), config().prefault(false)).unwrap();
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

/// Linux: a roll that finds its spare still being prepared creates the segment itself instead of waiting, and the
/// spare is the next roll's. The hold keeps the spare thread from finishing, so a roll that waited would never return.
#[cfg(target_os = "linux")]
#[test]
fn roll_does_not_wait_for_an_unfinished_spare() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), config()).unwrap();
    let hold = fault::hold_background(Point::SparePreallocate);
    for _ in 0..40 {
        append(&mut w, 1024);
    }
    assert!(hold.wait_reached(Duration::from_secs(10)));
    let prepared = identity(&dir.path().join(SPARE));
    // Two rolls pass it by; no second preparation starts, which would remove the first one's file.
    fill_until_roll(&mut w, dir.path(), 2);
    for _ in 0..40 {
        append(&mut w, 1024);
    }
    assert_eq!(identity(&dir.path().join(SPARE)), prepared);
    assert_ne!(identity(&newest_segment(dir.path())), prepared);
    hold.release();
    wait_until_spare_is_ready(&w, dir.path());
    fill_until_roll(&mut w, dir.path(), 1);
    assert_eq!(identity(&newest_segment(dir.path())), prepared);
    let total = w.next_seq();
    drop(w);
    assert_eq!(read_count(dir.path()), total);
}

/// The names of this process's threads that start with `tickertape-` (as `comm` keeps them), and its descriptors.
#[cfg(target_os = "linux")]
fn helpers_and_descriptors() -> (Vec<String>, usize) {
    let mut names: Vec<String> = std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|t| std::fs::read_to_string(t.unwrap().path().join("comm")).ok())
        .map(|n| n.trim_end().to_owned())
        .filter(|n| n.starts_with("tickertape-"))
        .collect();
    names.sort();
    (names, std::fs::read_dir("/proc/self/fd").unwrap().count())
}

/// Linux: with the option off, which is the default, a writer has no spare thread, no spare file and no descriptor
/// more than before 1.1 (ADR 0049); with it on it has one spare thread from `Writer::open`, and one descriptor more
/// while a spare is ready. This is the only test of this file that counts threads: the others hold `SERIAL` too.
#[cfg(target_os = "linux")]
#[test]
fn no_spare_by_default_on_linux() {
    let _serial = serial();
    assert!(!WriterConfig::default().spare_segment);
    let mut measured = Vec::new();
    for config in [small_config(), config()] {
        let on = config.spare_segment;
        let dir = tempfile::tempdir().unwrap();
        let mut w = Writer::open(dir.path(), config).unwrap();
        // Threads set their own names when they start: the prefault and write-back helpers, and the spare thread.
        let expected = if on {
            vec!["tickertape-pref", "tickertape-spar", "tickertape-writ"]
        } else {
            vec!["tickertape-pref", "tickertape-writ"]
        };
        wait_for(
            || helpers_and_descriptors().0 == expected,
            "the helper threads' names",
        );
        for _ in 0..40 {
            append(&mut w, 1024); // past half of the first segment
        }
        wait_for(|| w.spare_settled(), "the spare's preparation to end");
        assert_eq!(dir.path().join(SPARE).exists(), on);
        let (_, descriptors) = helpers_and_descriptors();
        measured.push(descriptors);
        fill_until_roll(&mut w, dir.path(), 2);
        let total = w.next_seq();
        drop(w);
        assert_eq!(read_count(dir.path()), total);
    }
    assert_eq!(measured[1], measured[0] + 1);
}

/// macOS: the spare is on by default, as before the option existed, and `spare_segment(false)` turns it off.
#[cfg(target_vendor = "apple")]
#[test]
fn spare_is_on_by_default_on_macos_and_can_be_refused() {
    let _serial = serial();
    assert!(WriterConfig::default().spare_segment);
    for (config, on) in [
        (small_config(), true),
        (small_config().spare_segment(false), false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Writer::open(dir.path(), config).unwrap();
        for _ in 0..40 {
            append(&mut w, 1024);
        }
        wait_for(|| w.spare_settled(), "the spare's preparation to end");
        assert_eq!(dir.path().join(SPARE).exists(), on);
        fill_until_roll(&mut w, dir.path(), 1);
    }
}

#[cfg(target_os = "linux")]
const ENOSPC: i32 = 28;

/// Linux: the roll finds its spare not ready, and creating a segment fails because the spare being prepared has taken
/// the last of the space. The roll then waits for the spare and uses it, instead of reporting a full disk (ADR 0049).
#[cfg(target_os = "linux")]
#[test]
fn a_roll_without_space_uses_the_spare_that_took_it() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), config()).unwrap();
    let hold = fault::hold_background(Point::SpareMap); // its blocks are allocated
    for _ in 0..40 {
        append(&mut w, 1024);
    }
    assert!(hold.wait_reached(Duration::from_secs(10)));
    let prepared = identity(&dir.path().join(SPARE));
    fault::fail_next(Point::Preallocate, ENOSPC); // the roll's own segment finds no space
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        hold.release();
    });
    fill_until_roll(&mut w, dir.path(), 1); // `append` panics on an error
    releaser.join().unwrap();
    fault::clear();
    assert_eq!(identity(&newest_segment(dir.path())), prepared);
    assert!(!dir.path().join(SPARE).exists());
    // The next segment gets a spare again.
    wait_for_ready_spare(&mut w, dir.path());
    fill_until_roll(&mut w, dir.path(), 1);
    let total = w.next_seq();
    drop(w);
    assert_eq!(read_count(dir.path()), total);
}

/// Linux: the roll and the spare run out of space together, and the spare then gives its space back (it fails after its
/// blocks were allocated). The roll tries once more and succeeds, as it would have without a spare.
#[cfg(target_os = "linux")]
#[test]
fn a_roll_tries_again_when_the_spare_gave_its_space_back() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), config()).unwrap();
    fault::fail_next_background(Point::SpareMap, 12 /* ENOMEM */);
    let hold = fault::hold_background(Point::SpareMap);
    for _ in 0..40 {
        append(&mut w, 1024);
    }
    assert!(hold.wait_reached(Duration::from_secs(10)));
    fault::fail_next(Point::Preallocate, ENOSPC); // the first try only
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        hold.release();
    });
    fill_until_roll(&mut w, dir.path(), 1); // `append` panics on an error
    releaser.join().unwrap();
    fault::clear();
    assert!(!dir.path().join(SPARE).exists());
    // The next segment gets a spare again.
    wait_for_ready_spare(&mut w, dir.path());
    let total = w.next_seq();
    drop(w);
    assert_eq!(read_count(dir.path()), total);
}

/// Both platforms: a roll that fails leaves the request for the spare as it was, as `main` did before the option
/// existed on macOS: a full disk does not get another preparation after each failed roll.
#[test]
fn a_failed_roll_asks_for_no_further_spare() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), config()).unwrap();
    fault::fail_next_background(Point::SparePreallocate, 28 /* ENOSPC */);
    for _ in 0..40 {
        append(&mut w, 1024); // past half: the spare is asked for and fails
    }
    wait_for(|| w.spare_settled(), "the spare's preparation to end");
    // Fill the segment until the record does not fit, so that the next append rolls; make the roll fail.
    fault::fail_next(Point::Preallocate, 28 /* ENOSPC */);
    let big = NewRecord {
        schema_id: 1,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload: &[0x5a; 1024],
    };
    let failed = loop {
        if let Err(e) = w.append(&big) {
            break e;
        }
    };
    assert!(failed.is_storage_full(), "{failed:?}");
    fault::clear();
    append(&mut w, 16); // still fits
    wait_for(|| w.spare_settled(), "the spare's preparation to end");
    assert!(
        !dir.path().join(SPARE).exists(),
        "the spare was prepared again after a failed roll"
    );
    // Once a roll succeeds, the next segment asks for one.
    fill_until_roll(&mut w, dir.path(), 1);
    wait_for_ready_spare(&mut w, dir.path());
    let total = w.next_seq();
    drop(w);
    assert_eq!(read_count(dir.path()), total);
}

/// Linux: the thread that prepares spares panics, with the blocks of its file allocated. Rolls go on, one that runs out
/// of space does not wait for it and frees the file it left (also when the writer is dropped), and so on to the end.
#[cfg(target_os = "linux")]
#[test]
fn a_panicked_spare_thread_neither_blocks_rolls_nor_keeps_its_space() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    // In a thread with a deadline: without the thread's end being recorded, the roll below would wait for ever.
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let spare = path.join(SPARE);
        let gone = || {
            let names = helpers_and_descriptors().0;
            !names.iter().any(|n| n == "tickertape-spar")
        };
        let started = |_: &Writer| {
            wait_for(
                || {
                    helpers_and_descriptors()
                        .0
                        .iter()
                        .any(|n| n == "tickertape-spar")
                },
                "the spare thread's name",
            );
        };
        let mut w = Writer::open(&path, config()).unwrap();
        started(&w);
        fault::fail_next_background(Point::SparePanic, 1);
        for _ in 0..40 {
            append(&mut w, 1024);
        }
        wait_for(gone, "the spare thread to panic");
        assert!(spare.exists(), "the panicked thread's file");
        fill_until_roll(&mut w, &path, 2); // rolls create their segments
        assert!(w.spare_settled());
        assert!(spare.exists());
        // A roll that finds no space: the left file is removed, and the roll tries again.
        fault::fail_next(Point::Preallocate, ENOSPC);
        fill_until_roll(&mut w, &path, 1);
        fault::clear();
        assert!(!spare.exists(), "the panicked thread's file was kept");
        fill_until_roll(&mut w, &path, 2); // no spare is asked for any more
        assert!(!spare.exists());
        let total = w.next_seq();
        drop(w);
        assert_eq!(read_count(&path), total);

        // Dropping the writer removes the file too.
        let mut w = Writer::open(&path, config()).unwrap();
        started(&w);
        fault::fail_next_background(Point::SparePanic, 1);
        for _ in 0..60 {
            append(&mut w, 1024);
        }
        wait_for(gone, "the spare thread to panic");
        assert!(spare.exists());
        drop(w);
        assert!(!spare.exists(), "drop kept the panicked thread's file");
        done.send(()).unwrap();
    });
    finished
        .recv_timeout(Duration::from_secs(60))
        .expect("a roll, or the drop, waits for a spare thread that has panicked");
}

/// Linux: if preparing that spare fails too, or none is being prepared, the roll reports the full disk as before.
#[cfg(target_os = "linux")]
#[test]
fn a_roll_without_space_or_spare_reports_storage_full() {
    let _serial = serial();
    for spare_fails in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let config = if spare_fails {
            config()
        } else {
            small_config()
        };
        let mut w = Writer::open(dir.path(), config).unwrap();
        let hold = spare_fails.then(|| {
            fault::fail_next_background(Point::SparePreallocate, ENOSPC);
            fault::hold_background(Point::SparePreallocate)
        });
        for _ in 0..40 {
            append(&mut w, 1024);
        }
        if let Some(hold) = &hold {
            assert!(hold.wait_reached(Duration::from_secs(10)));
        }
        // With a spare that was pending, the roll tries once more when the spare came to nothing.
        fault::fail_next(Point::Preallocate, ENOSPC);
        if spare_fails {
            fault::fail_next(Point::Preallocate, ENOSPC);
        }
        let releaser = hold.map(|hold| {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                hold.release();
            })
        });
        let record = NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns: 0,
            payload: &[0x5a; 1024],
        };
        let full = loop {
            if let Err(e) = w.append(&record) {
                break e;
            }
        };
        assert!(full.is_storage_full(), "{full:?}");
        if let Some(releaser) = releaser {
            releaser.join().unwrap();
        }
        fault::clear();
        // A later append retries the roll.
        fill_until_roll(&mut w, dir.path(), 1);
        let total = w.next_seq();
        drop(w);
        assert_eq!(read_count(dir.path()), total);
    }
}

/// A spare still being prepared when the writer is dropped is waited for and removed.
#[test]
fn dropping_the_writer_waits_for_the_spare_and_removes_it() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), config()).unwrap();
    let hold = fault::hold_background(Point::SparePreallocate);
    for _ in 0..40 {
        append(&mut w, 1024);
    }
    assert!(hold.wait_reached(Duration::from_secs(10)));
    let dropping = std::thread::spawn(move || drop(w));
    std::thread::sleep(Duration::from_millis(50));
    assert!(!dropping.is_finished(), "drop left the spare thread behind");
    hold.release();
    dropping.join().unwrap();
    assert!(!dir.path().join(SPARE).exists());
}
