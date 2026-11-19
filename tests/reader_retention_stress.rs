//! Crash contract C14, readers: readers racing with retention only ever see `Ok`, `Error::Discarded` (reads) or
//! `Error::SeqUnavailable` (`seek`), every record they return is valid and in order, and they keep making progress.
//!
//! A writer with 64 KiB segments checkpoints every 128 records with `max_segments(2)` retention, so the oldest
//! segment disappears many times a second. Against it run safe and zero-copy readers: fast and slow followers, a
//! durable-only follower, readers that `seek` near the oldest record in a tight loop, readers that reopen in a loop,
//! and `inspect::list_segments`/`inspect::verify` in a loop. A few seconds in the default run; longer with
//! `--ignored`. Replay a run with `TICKERTAPE_TEST_SEED`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tickertape::retention::{Action, Policy};
use tickertape::{Error, Reader, ReaderConfig, Writer, inspect};
use tickertape_tests::{Rng, append_n, check_record, small_config};

/// A reader that hasn't made progress for this long while the writer has is stuck.
const STALL: Duration = Duration::from_secs(5);

#[test]
fn readers_under_retention() {
    stress(Duration::from_secs(3));
}

#[test]
#[ignore = "long: one minute of readers racing with retention; run with --release -- --ignored"]
fn readers_under_retention_long() {
    stress(Duration::from_secs(60));
}

#[derive(Debug, Clone, Copy)]
enum Role {
    Follow {
        mapped: bool,
        slow: bool,
        durable: bool,
    },
    Seek {
        mapped: bool,
    },
    Reopen {
        mapped: bool,
    },
    Inspect,
}

const ROLES: [Role; 10] = [
    Role::Follow {
        mapped: false,
        slow: false,
        durable: false,
    },
    Role::Follow {
        mapped: true,
        slow: false,
        durable: false,
    },
    Role::Follow {
        mapped: false,
        slow: true,
        durable: false,
    },
    Role::Follow {
        mapped: true,
        slow: true,
        durable: false,
    },
    Role::Follow {
        mapped: false,
        slow: true,
        durable: true,
    },
    Role::Seek { mapped: false },
    Role::Seek { mapped: true },
    Role::Reopen { mapped: false },
    Role::Reopen { mapped: true },
    Role::Inspect,
];

struct Shared<'a> {
    dir: &'a std::path::Path,
    stop: &'a AtomicBool,
    /// The writer's next sequence number after its last checkpoint.
    head: &'a AtomicU64,
    /// `Error::Discarded` and `Error::SeqUnavailable` seen by all readers.
    discarded: &'a AtomicU64,
    unavailable: &'a AtomicU64,
}

/// Sets the stop flag when dropped, so that a panic on any thread ends the others too.
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn stress(duration: Duration) {
    let dir = tempfile::tempdir().unwrap();
    let stop = AtomicBool::new(false);
    let head = AtomicU64::new(0);
    let discarded = AtomicU64::new(0);
    let unavailable = AtomicU64::new(0);
    let progress: Vec<AtomicU64> = ROLES.iter().map(|_| AtomicU64::new(0)).collect();
    let mut rng = Rng::from_env("readers_under_retention");
    let config = small_config().retention(Policy::default().max_segments(2), Action::Delete);
    let mut writer = Writer::open(dir.path(), config).unwrap();
    append_n(&mut writer, 64);
    writer.checkpoint().unwrap();
    head.store(writer.next_seq(), Ordering::SeqCst);
    let shared = Shared {
        dir: dir.path(),
        stop: &stop,
        head: &head,
        discarded: &discarded,
        unavailable: &unavailable,
    };

    let removed = thread::scope(|s| {
        let _stop = StopOnDrop(&stop);
        let writer = s.spawn(|| {
            let _stop = StopOnDrop(&stop);
            write(&mut writer, &shared)
        });
        for (role, progress) in ROLES.iter().zip(&progress) {
            let seed = rng.next_u64();
            let shared = &shared;
            s.spawn(move || {
                let _stop = StopOnDrop(shared.stop);
                let mut rng = Rng::new(seed);
                match *role {
                    Role::Follow {
                        mapped,
                        slow,
                        durable,
                    } => follow(shared, progress, mapped, slow, durable),
                    Role::Seek { mapped } => seek_loop(shared, progress, mapped, &mut rng),
                    Role::Reopen { mapped } => reopen_loop(shared, progress, mapped, &mut rng),
                    Role::Inspect => inspect_loop(shared, progress),
                }
            });
        }

        // Every reader must keep making progress while the writer does.
        let started = Instant::now();
        let mut last: Vec<(u64, Instant)> = progress.iter().map(|_| (0, started)).collect();
        while started.elapsed() < duration && !stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(50));
            let now = Instant::now();
            for (i, (seen, at)) in last.iter_mut().enumerate() {
                let p = progress[i].load(Ordering::SeqCst);
                if p != *seen {
                    *seen = p;
                    *at = now;
                }
                assert!(
                    now.duration_since(*at) < STALL,
                    "{:?} made no progress for {STALL:?} (at {p})",
                    ROLES[i]
                );
            }
        }
        stop.store(true, Ordering::SeqCst);
        writer.join().expect("writer thread")
    });
    let (discarded, unavailable) = (discarded.into_inner(), unavailable.into_inner());
    eprintln!(
        "readers_under_retention: {} records, {removed} segments removed, {discarded} discarded, \
         {unavailable} unavailable; progress {:?}",
        head.load(Ordering::SeqCst),
        progress
            .iter()
            .map(|p| p.load(Ordering::SeqCst))
            .collect::<Vec<_>>()
    );
    assert!(removed >= 3, "retention removed only {removed} segments");
    assert!(
        discarded > 0 && unavailable > 0,
        "readers never raced with retention"
    );
}

/// Appends and checkpoints (running retention) until stopped. Returns the number of segments retention removed.
fn write(writer: &mut Writer, shared: &Shared<'_>) -> u64 {
    while !shared.stop.load(Ordering::SeqCst) {
        append_n(writer, 128);
        writer.checkpoint().unwrap();
        if let Some(e) = writer.last_retention_error() {
            panic!("retention failed: {e}");
        }
        shared.head.store(writer.next_seq(), Ordering::SeqCst);
    }
    writer.retention_report().removed_segments
}

fn open(dir: &std::path::Path, mapped: bool, durable: bool) -> Reader {
    let config = ReaderConfig::default().durable_only(durable);
    let reader = if mapped {
        // SAFETY: only this process's tickertape writer modifies the journal while the reader exists.
        unsafe { Reader::open_mapped(dir, config) }
    } else {
        Reader::open(dir, config)
    };
    reader.unwrap_or_else(|e| panic!("open (mapped {mapped}): {e:?}"))
}

/// Seeks to `target`, or to the oldest record if `target` is gone. Returns where the reader landed.
fn reposition(shared: &Shared<'_>, reader: &mut Reader, mut target: u64) -> u64 {
    loop {
        match reader.seek(target) {
            Ok(()) => return target,
            Err(Error::SeqUnavailable { requested, first }) => {
                shared.unavailable.fetch_add(1, Ordering::SeqCst);
                assert_eq!(requested, target);
                assert!(first > target, "first {first} <= requested {target}");
                target = first;
            }
            Err(e) => panic!("seek({target}): {e:?}"),
        }
    }
}

/// Follows the journal, seeking past discarded history.
fn follow(shared: &Shared<'_>, progress: &AtomicU64, mapped: bool, slow: bool, durable: bool) {
    let mut reader = open(shared.dir, mapped, durable);
    let mut expected = reader.position();
    while !shared.stop.load(Ordering::SeqCst) {
        match reader.next_timeout(Some(Duration::from_millis(20))) {
            Ok(Some(record)) => {
                let record = record.to_owned();
                assert_eq!(record.seq, expected, "mapped {mapped} durable {durable}");
                check_record(&record);
                expected += 1;
                progress.fetch_add(1, Ordering::SeqCst);
                if slow {
                    thread::sleep(Duration::from_millis(2));
                }
            }
            Ok(None) => {}
            Err(Error::Discarded { seq }) => {
                shared.discarded.fetch_add(1, Ordering::SeqCst);
                assert_eq!(seq, expected);
                expected = reposition(shared, &mut reader, seq);
            }
            Err(e) => panic!("follow (mapped {mapped} durable {durable}): {e:?}"),
        }
    }
}

/// Seeks near the oldest retained record in a tight loop, reading a few records after each seek.
fn seek_loop(shared: &Shared<'_>, progress: &AtomicU64, mapped: bool, rng: &mut Rng) {
    let mut reader = open(shared.dir, mapped, false);
    while !shared.stop.load(Ordering::SeqCst) {
        // Two segments hold about 700 records: aim at the oldest end and just before it.
        let target = shared
            .head
            .load(Ordering::SeqCst)
            .saturating_sub(300 + rng.below(1200));
        match reader.seek(target) {
            Ok(()) => {
                progress.fetch_add(1, Ordering::SeqCst);
                for i in 0..rng.below(4) {
                    match reader.try_next() {
                        Ok(Some(record)) => {
                            let record = record.to_owned();
                            assert_eq!(record.seq, target + i, "mapped {mapped}");
                            check_record(&record);
                        }
                        Ok(None) => break,
                        Err(Error::Discarded { seq }) => {
                            shared.discarded.fetch_add(1, Ordering::SeqCst);
                            assert_eq!(seq, target + i);
                            break;
                        }
                        Err(e) => panic!("read after seek({target}) (mapped {mapped}): {e:?}"),
                    }
                }
            }
            Err(Error::SeqUnavailable { requested, first }) => {
                shared.unavailable.fetch_add(1, Ordering::SeqCst);
                assert_eq!(requested, target);
                assert!(first > target, "first {first} <= requested {target}");
                progress.fetch_add(1, Ordering::SeqCst);
            }
            Err(e) => panic!("seek({target}) (mapped {mapped}): {e:?}"),
        }
    }
}

/// Opens a new reader in a loop and reads a few records from the oldest segment.
fn reopen_loop(shared: &Shared<'_>, progress: &AtomicU64, mapped: bool, rng: &mut Rng) {
    while !shared.stop.load(Ordering::SeqCst) {
        let mut reader = open(shared.dir, mapped, false);
        let first = reader.position();
        progress.fetch_add(1, Ordering::SeqCst);
        for i in 0..rng.below(8) {
            match reader.try_next() {
                Ok(Some(record)) => {
                    let record = record.to_owned();
                    assert_eq!(record.seq, first + i, "mapped {mapped}");
                    check_record(&record);
                }
                Ok(None) => break,
                Err(Error::Discarded { seq }) => {
                    shared.discarded.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(seq, first + i);
                    break;
                }
                Err(e) => panic!("read after open (mapped {mapped}): {e:?}"),
            }
        }
    }
}

/// Lists and verifies the journal in a loop: no errors, and no problems in a healthy journal.
fn inspect_loop(shared: &Shared<'_>, progress: &AtomicU64) {
    while !shared.stop.load(Ordering::SeqCst) {
        let listed = inspect::list_segments(shared.dir).unwrap_or_else(|e| panic!("list: {e:?}"));
        assert!(listed.windows(2).all(|w| w[0].base_seq < w[1].base_seq));
        let report = inspect::verify(shared.dir).unwrap_or_else(|e| panic!("verify: {e:?}"));
        assert!(report.problems.is_empty(), "verify: {:?}", report.problems);
        assert!(!report.segments.is_empty());
        progress.fetch_add(1, Ordering::SeqCst);
    }
}
