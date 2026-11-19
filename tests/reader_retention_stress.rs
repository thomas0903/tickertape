//! Crash contract C14, readers: readers racing with retention only ever see `Ok`, `Error::Discarded` (reads) or
//! `Error::SeqUnavailable` (`seek`), every record they return is valid and in order, and they keep making progress.
//!
//! A writer with 64 KiB segments checkpoints every 128 records with `max_segments(2)` retention, so the oldest
//! segment disappears many times a second. Against it run safe and zero-copy readers: fast and slow followers, a
//! durable-only follower, readers that `seek` near the oldest record in a tight loop, readers that reopen in a loop,
//! and `inspect::list_segments`/`inspect::verify` in a loop. A few seconds in the default run (longer if a slow disk
//! stalls checkpoints, until retention has removed segments and readers have seen both errors); one minute with
//! `--ignored`. Replay a run with `TICKERTAPE_TEST_SEED`.
//!
//! `durable_readers_under_repairs` adds crash contract C13 (ADR 0034): the writer restarts every few checkpoints, and
//! before most restarts a record after the checkpoint is damaged. Usually it is corrupted in a copy renamed over its
//! segment, so recovery replaces a segment that readers still hold with valid bytes; sometimes its commit word is
//! zeroed in the file itself, so recovery removes it and the records after it in place, while safe readers may still
//! hold the old bytes in their read buffers. Durable-only readers must never return a record that a reader of the
//! repaired journal doesn't also return at the same sequence number with the same bytes; visible readers must never
//! move from a removed history into the repaired one without `Error::Discarded`, except across an in-place repair,
//! which keeps the file's identity (C11). A zero-copy visible reader's payload borrows the mapping, so an in-place
//! repair that removes a record it has just returned can zero that payload before the test copies it out, as the
//! crash contract says; only that reader, and only for such a record, may return a changed payload.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tickertape::retention::{Action, Policy};
use tickertape::{
    Error, NewRecord, OwnedRecord, Reader, ReaderConfig, RecoveryAction, Writer, inspect,
};
use tickertape_tests::{
    Rng, append_n, check_record, corrupt_by_replacement, end_of_records, payload, payload_len,
    reopen_writer, segments, small_config, zero_commit_word,
};

/// A reader that hasn't made progress for this long while the writer has is stuck. Time during which the writer
/// itself makes no progress (a slow checkpoint) doesn't count.
const STALL: Duration = Duration::from_secs(5);
/// How long a run may continue past its duration until retention and both errors have been exercised (a slow
/// disk can stall checkpoints, and so retention, for seconds).
const GRACE: Duration = Duration::from_secs(60);

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
    /// Segments removed by the writer's retention so far.
    removed: &'a AtomicU64,
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
    let removed = AtomicU64::new(0);
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
        removed: &removed,
    };

    thread::scope(|s| {
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

        // Every reader must keep making progress while the writer does: count, per reader, the time without
        // progress during which the writer's head moved.
        let started = Instant::now();
        let exercised = || {
            removed.load(Ordering::SeqCst) >= 3
                && discarded.load(Ordering::SeqCst) > 0
                && unavailable.load(Ordering::SeqCst) > 0
        };
        let mut seen: Vec<u64> = progress.iter().map(|_| 0).collect();
        let mut stalled = vec![Duration::ZERO; ROLES.len()];
        let (mut last_head, mut last_sample) = (head.load(Ordering::SeqCst), started);
        while (started.elapsed() < duration
            || (!exercised() && started.elapsed() < duration + GRACE))
            && !stop.load(Ordering::SeqCst)
        {
            thread::sleep(Duration::from_millis(50));
            let now = Instant::now();
            let head_now = head.load(Ordering::SeqCst);
            let writer_moved = head_now != last_head;
            for (i, role) in ROLES.iter().enumerate() {
                let p = progress[i].load(Ordering::SeqCst);
                if p != seen[i] {
                    seen[i] = p;
                    stalled[i] = Duration::ZERO;
                } else if writer_moved {
                    stalled[i] += now - last_sample;
                }
                assert!(
                    stalled[i] < STALL,
                    "{role:?} made no progress for {STALL:?} while the writer did (at {p})"
                );
            }
            (last_head, last_sample) = (head_now, now);
        }
        stop.store(true, Ordering::SeqCst);
        writer.join().expect("writer thread");
    });
    let removed = removed.into_inner();
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
    assert!(
        removed >= 3,
        "retention removed only {removed} segments in {:?} (writer too slow on this disk?)",
        duration + GRACE
    );
    assert!(
        discarded > 0 && unavailable > 0,
        "readers never raced with retention"
    );
}

/// Appends and checkpoints (running retention) until stopped.
fn write(writer: &mut Writer, shared: &Shared<'_>) {
    while !shared.stop.load(Ordering::SeqCst) {
        append_n(writer, 128);
        writer.checkpoint().unwrap();
        if let Some(e) = writer.last_retention_error() {
            panic!("retention failed: {e}");
        }
        shared.head.store(writer.next_seq(), Ordering::SeqCst);
        shared
            .removed
            .store(writer.retention_report().removed_segments, Ordering::SeqCst);
    }
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

// Repairs: corruption between writer restarts (C13, ADR 0034).

#[test]
fn durable_readers_under_repairs() {
    repairs(Duration::from_secs(3));
}

#[test]
#[ignore = "long: one minute of readers racing with repairs; run with --release -- --ignored"]
fn durable_readers_under_repairs_long() {
    repairs(Duration::from_secs(60));
}

/// The payload generation `generation` of the writer (the number of restarts before it) writes at `seq`, so that
/// histories written by different generations differ.
fn generation_payload(seq: u64, generation: u32) -> Vec<u8> {
    payload(seq ^ (u64::from(generation) << 40), payload_len(seq))
}

/// Appends `n` records of generation `generation` (its number is the schema id).
fn append_generation(writer: &mut Writer, n: u64, generation: u32) {
    for _ in 0..n {
        let seq = writer.next_seq();
        writer
            .append(&NewRecord {
                schema_id: generation,
                venue_ts_ns: seq * 10,
                local_ts_ns: seq * 10 + 1,
                payload: &generation_payload(seq, generation),
            })
            .expect("append");
    }
}

/// Checks that a record is what generation `schema_id` wrote at its sequence number.
fn check_generation(r: &OwnedRecord) {
    assert_eq!(r.venue_ts_ns, r.seq * 10);
    assert_eq!(r.local_ts_ns, r.seq * 10 + 1);
    assert_eq!(
        r.payload,
        generation_payload(r.seq, r.schema_id),
        "payload of {}",
        r.seq
    );
}

/// The generation whose record is at `seq` in the journal as of `starts` (`starts[g]` is where generation `g` began
/// writing): the last generation that began at or before `seq`. Each later generation began after `seq`, so it kept
/// that record.
fn current_generation(starts: &[u64], seq: u64) -> u32 {
    starts
        .iter()
        .rposition(|&start| start <= seq)
        .expect("generation 0 starts at 0") as u32
}

struct Repairs<'a> {
    dir: &'a std::path::Path,
    stop: &'a AtomicBool,
    /// `starts[g]`: the sequence number at which writer generation `g` began appending (its `next_seq` after
    /// recovery). Pushed before the generation appends anything.
    starts: &'a Mutex<Vec<u64>>,
    /// `restarts[g]`: how the restart before generation `g` changed the segment files. Pushed before `starts[g]`.
    restarts: &'a Mutex<Vec<Restart>>,
    /// Odd while the writer is stopped, from just before a corrupt copy may be renamed into place until recovery has
    /// repaired it. Visible readers may report corruption only during such a window.
    window: &'a AtomicU64,
    /// Restarts whose recovery replaced or quarantined a segment.
    repairs: &'a AtomicU64,
    /// Restarts whose recovery removed records in place.
    in_place_repairs: &'a AtomicU64,
    /// `Error::Discarded` seen by durable-only readers.
    discarded: &'a AtomicU64,
    /// Records returned by durable-only readers.
    durable_records: &'a AtomicU64,
    /// Records the zero-copy visible reader returned whose payload an in-place repair changed under it.
    changed_payloads: &'a AtomicU64,
}

/// How a writer restart changed the segment files, as far as a reader holding them open can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Restart {
    /// No repair: every file keeps its identity and every record.
    Clean,
    /// Recovery removed records in place after a commit word zeroed in the file, and the new writer began there: that
    /// file keeps its identity, so readers can't notice (C11). Later segments, if any, were quarantined.
    InPlace,
    /// Recovery began the new writer in a segment it replaced (a corrupt copy was renamed over it), or after
    /// quarantining segments: readers holding the old files notice, and must report `Error::Discarded` before they
    /// return records of the repaired history.
    Replaced,
}

fn repairs(duration: Duration) {
    let dir = tempfile::tempdir().unwrap();
    let stop = AtomicBool::new(false);
    let starts = Mutex::new(vec![0]);
    let restarts = Mutex::new(vec![Restart::Clean]);
    let window = AtomicU64::new(0);
    let (repairs, in_place_repairs, discarded, durable_records, changed_payloads) = (
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    );
    let mut rng = Rng::from_env("durable_readers_under_repairs");
    let shared = Repairs {
        dir: dir.path(),
        stop: &stop,
        starts: &starts,
        restarts: &restarts,
        window: &window,
        repairs: &repairs,
        in_place_repairs: &in_place_repairs,
        discarded: &discarded,
        durable_records: &durable_records,
        changed_payloads: &changed_payloads,
    };
    let config = small_config().retention(Policy::default().max_segments(6), Action::Delete);
    let mut writer = Writer::open(dir.path(), config.clone()).unwrap();
    append_generation(&mut writer, 64, 0);
    writer.checkpoint().unwrap();

    // (mapped, slow) for durable-only readers, then mapped for visible readers.
    let durable = [(false, false), (true, false), (false, true)];
    let visible = [false, true];
    let runs = thread::scope(|s| {
        let _stop = StopOnDrop(&stop);
        let writer_seed = rng.next_u64();
        let (stop, shared) = (&stop, &shared);
        let writer_thread = s.spawn(move || {
            let _stop = StopOnDrop(stop);
            restart_loop(writer, config, shared, Rng::new(writer_seed))
        });
        let durable: Vec<_> = durable
            .iter()
            .map(|&(mapped, slow)| {
                s.spawn(move || {
                    let _stop = StopOnDrop(shared.stop);
                    durable_follow(shared, mapped, slow)
                })
            })
            .collect();
        let visible: Vec<_> = visible
            .iter()
            .map(|&mapped| {
                s.spawn(move || {
                    let _stop = StopOnDrop(shared.stop);
                    visible_follow(shared, mapped)
                })
            })
            .collect();

        let started = Instant::now();
        let exercised = || {
            repairs.load(Ordering::SeqCst) >= 3
                && in_place_repairs.load(Ordering::SeqCst) > 0
                && discarded.load(Ordering::SeqCst) > 0
        };
        while (started.elapsed() < duration
            || (!exercised() && started.elapsed() < duration + GRACE))
            && !stop.load(Ordering::SeqCst)
        {
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
        writer_thread.join().expect("writer thread");
        for v in visible {
            v.join().expect("visible reader");
        }
        durable
            .into_iter()
            .map(|d| d.join().expect("durable-only reader"))
            .collect::<Vec<_>>()
    });

    // Every record a durable-only reader returned is the one the final journal holds at its sequence number: the
    // record of the last generation that began at or before it.
    let starts = starts.into_inner().unwrap();
    for (reader, runs) in runs.iter().enumerate() {
        for &(first, end, generation) in runs {
            let current = current_generation(&starts, first);
            let overtaken = starts[generation as usize + 1..]
                .iter()
                .position(|&start| start < end);
            assert!(
                current == generation && overtaken.is_none(),
                "durable-only reader {reader} returned records {first}..{end} of generation {generation}, \
                 but generation {} began at {} and removed them (starts {:?})",
                overtaken.map_or(current as usize, |g| generation as usize + 1 + g),
                overtaken.map_or(starts[current as usize], |g| starts
                    [generation as usize + 1 + g]),
                &starts[..starts.len().min(64)],
            );
        }
    }
    // And that is what a reader of the repaired journal returns, for the part retention kept.
    let mut reader = Reader::open(dir.path(), ReaderConfig::default()).unwrap();
    let mut journal = Vec::new();
    while let Some(record) = reader.next_owned().unwrap() {
        check_generation(&record);
        assert_eq!(
            record.schema_id,
            current_generation(&starts, record.seq),
            "record {}",
            record.seq
        );
        journal.push((record.seq, record.schema_id));
    }
    let first = journal.first().map_or(u64::MAX, |r| r.0);
    for runs in &runs {
        for &(start, end, generation) in runs {
            for seq in start.max(first)..end {
                let (at, current) = journal[(seq - first) as usize];
                assert_eq!(
                    (at, current),
                    (seq, generation),
                    "durable-only record {seq}"
                );
            }
        }
    }
    let (repairs, in_place_repairs, discarded) = (
        repairs.into_inner(),
        in_place_repairs.into_inner(),
        discarded.into_inner(),
    );
    eprintln!(
        "durable_readers_under_repairs: {} generations, {repairs} repairs, {in_place_repairs} in place, {} \
         durable-only records, {discarded} discarded, {} zero-copy payloads changed under the reader",
        starts.len(),
        durable_records.into_inner(),
        changed_payloads.into_inner(),
    );
    assert!(repairs >= 3, "only {repairs} repairs");
    assert!(in_place_repairs > 0, "no in-place repairs");
    assert!(
        discarded > 0,
        "durable-only readers never raced with a repair"
    );
}

/// Appends and checkpoints, then appends a tail that isn't checkpointed and restarts the writer, usually after
/// damaging a record after the checkpoint: half the time by corrupting it in a copy renamed over its segment, a
/// quarter of the time by zeroing its commit word in place. Until stopped.
fn restart_loop(
    mut writer: Writer,
    config: tickertape::WriterConfig,
    shared: &Repairs<'_>,
    mut rng: Rng,
) {
    let mut generation = 0;
    while !shared.stop.load(Ordering::SeqCst) {
        for _ in 0..1 + rng.below(8) {
            append_generation(&mut writer, 128, generation);
            writer.checkpoint().unwrap();
        }
        append_generation(&mut writer, 1 + rng.below(300), generation);
        shared.window.fetch_add(1, Ordering::SeqCst);
        drop(writer);
        let damage = rng.below(8);
        let (mut zeroed, mut replaced) = (false, false);
        if damage >= 2 {
            let marker = inspect::read_checkpoint(shared.dir)
                .unwrap()
                .expect("checkpoint");
            let newest = segments(shared.dir).pop().expect("a segment").1;
            let (_, end) = end_of_records(&newest);
            if end > marker.next_seq {
                let seq = marker.next_seq + rng.below(end - marker.next_seq);
                if damage < 4 {
                    zero_commit_word(shared.dir, seq);
                    zeroed = true;
                } else {
                    corrupt_by_replacement(shared.dir, seq);
                    replaced = true;
                }
            }
        }
        writer = reopen_writer(shared.dir, config.clone());
        let actions = &writer.recovery_report().actions;
        if actions.iter().any(|a| {
            matches!(
                a,
                RecoveryAction::RepairedTail { .. } | RecoveryAction::QuarantinedSegment { .. }
            )
        }) {
            shared.repairs.fetch_add(1, Ordering::SeqCst);
            replaced = true;
        }
        if zeroed {
            assert!(
                actions
                    .iter()
                    .any(|a| matches!(a, RecoveryAction::ZeroedUncommitted { .. })),
                "{actions:?}"
            );
            shared.in_place_repairs.fetch_add(1, Ordering::SeqCst);
        }
        generation += 1;
        // A zeroed commit word is where recovery stopped and the new writer began, in place, even when recovery also
        // quarantined later segments (the zeroed word was in an older one).
        let restart = match (zeroed, replaced) {
            (true, _) => Restart::InPlace,
            (false, true) => Restart::Replaced,
            (false, false) => Restart::Clean,
        };
        shared.restarts.lock().unwrap().push(restart);
        shared.starts.lock().unwrap().push(writer.next_seq());
        shared.window.fetch_add(1, Ordering::SeqCst);
    }
}

/// Seeks to `target`, or to the oldest record if retention removed it. Returns where the reader landed.
fn seek_or_oldest(reader: &mut Reader, mut target: u64) -> u64 {
    loop {
        match reader.seek(target) {
            Ok(()) => return target,
            Err(Error::SeqUnavailable { first, .. }) if first > target => target = first,
            Err(e) => panic!("seek({target}): {e:?}"),
        }
    }
}

/// Follows the journal with a durable-only reader, seeking past discarded history. Returns the records it returned
/// as runs `(first, end, generation)` of consecutive sequence numbers of one generation.
fn durable_follow(shared: &Repairs<'_>, mapped: bool, slow: bool) -> Vec<(u64, u64, u32)> {
    let mut reader = open(shared.dir, mapped, true);
    let mut expected = reader.position();
    let mut runs: Vec<(u64, u64, u32)> = Vec::new();
    while !shared.stop.load(Ordering::SeqCst) {
        match reader.next_timeout(Some(Duration::from_millis(20))) {
            Ok(Some(record)) => {
                let record = record.to_owned();
                assert_eq!(record.seq, expected, "mapped {mapped}");
                check_generation(&record);
                match runs.last_mut() {
                    Some((_, end, generation))
                        if *end == record.seq && *generation == record.schema_id =>
                    {
                        *end += 1;
                    }
                    _ => runs.push((record.seq, record.seq + 1, record.schema_id)),
                }
                expected += 1;
                shared.durable_records.fetch_add(1, Ordering::SeqCst);
                if slow {
                    thread::sleep(Duration::from_millis(2));
                }
            }
            Ok(None) => {}
            Err(Error::Discarded { seq }) => {
                shared.discarded.fetch_add(1, Ordering::SeqCst);
                assert_eq!(seq, expected);
                expected = seek_or_oldest(&mut reader, seq);
            }
            Err(e) => panic!("durable-only follow (mapped {mapped}): {e:?}"),
        }
    }
    runs
}

/// Follows the journal with a visible reader. Between two `Error::Discarded`, consecutive records must belong to one
/// history: if record `s` is of generation `g1` and `s + 1` of `g2`, then `g2 >= g1` and every generation after `g1`
/// up to `g2` began at or after `s + 1` (kept record `s`) or kept the files' identities (an in-place repair, or a
/// restart without a repair). A visible reader can't see an in-place repair (the file keeps its identity): it may
/// return the removed records it had already read or buffered and then continue in the new history, which later
/// restarts without a repair extend in the same file. Records at a sequence number have the same length in every
/// generation here, so its cursor stays on a record boundary; with other lengths it could fall inside a record of
/// the new history (`docs/crash-contract.md`, C11).
fn visible_follow(shared: &Repairs<'_>, mapped: bool) {
    let mut reader = open(shared.dir, mapped, false);
    let mut expected = reader.position();
    let mut previous: Option<(u64, u32)> = None;
    while !shared.stop.load(Ordering::SeqCst) {
        let window = shared.window.load(Ordering::SeqCst);
        match reader.next_timeout(Some(Duration::from_millis(20))) {
            Ok(Some(record)) => {
                let record = record.to_owned();
                assert_eq!(record.seq, expected, "mapped {mapped}");
                if mapped {
                    check_mapped_visible(shared, &record, window);
                } else {
                    check_generation(&record);
                }
                if let Some((seq, generation)) = previous {
                    let next = record.schema_id;
                    let starts = shared.starts.lock().unwrap();
                    let restarts = shared.restarts.lock().unwrap();
                    let between = generation as usize + 1..=next as usize;
                    assert!(
                        next >= generation
                            && between
                                .clone()
                                .all(|g| starts[g] > seq || restarts[g] != Restart::Replaced),
                        "visible reader (mapped {mapped}) moved from record {seq} of generation {generation} to \
                         record {} of generation {next} without Error::Discarded (generations between: {:?})",
                        record.seq,
                        between
                            .map(|g| (g, starts[g], restarts[g]))
                            .collect::<Vec<_>>(),
                    );
                }
                previous = Some((record.seq, record.schema_id));
                expected += 1;
            }
            Ok(None) => {}
            Err(Error::Discarded { seq }) => {
                assert_eq!(seq, expected);
                expected = seek_or_oldest(&mut reader, seq);
                previous = None;
            }
            // A visible reader that opened the corrupt copy before recovery replaced it reports the corruption
            // (C11). Retry: after the repair, the identity check turns it into `Error::Discarded`.
            Err(Error::Corrupt { .. })
                if window % 2 == 1 || shared.window.load(Ordering::SeqCst) != window =>
            {
                thread::sleep(Duration::from_millis(1));
            }
            Err(e) => panic!("visible follow (mapped {mapped}): {e:?}"),
        }
    }
}

/// Checks a record the zero-copy visible reader returned, read while `window` was the repair window.
///
/// Its payload borrows the mapping, and the one change a tickertape writer makes to committed bytes is recovery
/// zeroing, in place, the records after a commit word that storage damage emptied (C11): `docs/crash-contract.md`
/// says that "a slice a zero-copy reader already holds for such a record can change". So a record that passed
/// validation can come out of `to_owned` with its payload zeroed, wholly or in part (the copy races with the
/// zeroing), or, if the copy is late enough, partly overwritten by the new history appended there, while its header
/// fields, copied out during validation, are right. That is all the contract allows, and only for a record such a
/// repair removed while the reader held it: one at or after the start of a generation that began during this read
/// with an in-place repair. The safe reader, and durable-only readers of both kinds (which return only records below
/// a marker, never zeroed in place: C13), get the full check.
fn check_mapped_visible(shared: &Repairs<'_>, r: &OwnedRecord, window: u64) {
    if r.payload == generation_payload(r.seq, r.schema_id) {
        return check_generation(r);
    }
    assert_eq!(r.venue_ts_ns, r.seq * 10);
    assert_eq!(r.local_ts_ns, r.seq * 10 + 1);
    // Wait for the restart in progress, if any, to record how it began. `restart_loop` checks `stop` only between
    // restarts.
    let deadline = Instant::now() + GRACE;
    while shared.window.load(Ordering::SeqCst) % 2 == 1 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
    // The restart in progress when the read began, or else the next one, made generation `window / 2 + 1`.
    let since = (window / 2 + 1) as usize;
    let starts = shared.starts.lock().unwrap();
    let restarts = shared.restarts.lock().unwrap();
    let generations: Vec<_> = (since..starts.len())
        .map(|g| (g, starts[g], restarts[g]))
        .collect();
    assert!(
        generations
            .iter()
            .any(|&(_, start, restart)| restart == Restart::InPlace && start <= r.seq),
        "zero-copy payload of {} (generation {}) changed, but no in-place repair removed it during the read \
         (generations since: {generations:?}): {:?}",
        r.seq,
        r.schema_id,
        r.payload
    );
    // Each byte is the validated one, zero, or what a later generation wrote there.
    let written: Vec<_> = std::iter::once(r.schema_id)
        .chain(generations.iter().map(|&(g, _, _)| g as u32))
        .map(|g| generation_payload(r.seq, g))
        .collect();
    assert!(
        (0..r.payload.len())
            .all(|i| r.payload[i] == 0 || written.iter().any(|w| w.get(i) == Some(&r.payload[i])))
            && r.payload.len() == written[0].len(),
        "zero-copy payload of {} (generation {}) is neither zeroed nor written by generations {generations:?}: {:?}",
        r.seq,
        r.schema_id,
        r.payload
    );
    shared.changed_payloads.fetch_add(1, Ordering::SeqCst);
}
