//! Crash contract C1, C2, C3, C4: kill the writer process with SIGKILL at random points (including during recovery
//! and during segment rolls), then reopen and verify, many times over.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tickertape::{Reader, ReaderConfig, RecoveryAction, Writer, WriterConfig};
use tickertape_tests::{Rng, kill_len, mixed_len, payload, read_all, segments};

const KILL_WRITER: &str = env!("CARGO_BIN_EXE_tt-kill-writer");
const SEGMENT: u64 = 64 * 1024;

fn kill_loop(iterations: u32, slow_fill: bool) {
    let mut rng = Rng::from_env("kill_loop");
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal");
    // Live readers (safe and zero-copy) that stay open across every crash: they must never see a gap, corruption, or
    // a discarded segment.
    let mut reader: Option<Reader> = None;
    let mut mapped: Option<Reader> = None;
    let mut killed_mid_run = 0;
    let mut torn_zeroed = 0;

    for i in 0..iterations {
        let mut child = Command::new(KILL_WRITER)
            .args([
                journal.to_str().unwrap(),
                &SEGMENT.to_string(),
                &rng.next_u64().to_string(),
            ])
            .args(slow_fill.then_some("slow-fill"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn writer");
        // Odd iterations: kill at a random moment after spawning (often during open/recovery). Even iterations: wait
        // until appends have started, then kill a little later (often during appends and rolls).
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut out = String::new();
        if i % 2 == 0 {
            stdout.read_line(&mut out).unwrap();
            std::thread::sleep(Duration::from_micros(rng.below(5_000)));
        } else {
            std::thread::sleep(Duration::from_micros(rng.below(20_000)));
        }
        child.kill().expect("SIGKILL");
        child.wait().unwrap();
        stdout.read_to_string(&mut out).unwrap();
        let last_reported: Option<u64> = out.lines().filter_map(|l| l.parse().ok()).next_back();
        if last_reported.is_some() {
            killed_mid_run += 1;
        }

        if !journal.join("00000000000000000000.tape").exists() {
            continue; // killed before the journal was created
        }
        let w = Writer::open(&journal, WriterConfig::default().segment_size(SEGMENT))
            .unwrap_or_else(|e| panic!("iteration {i}: recovery failed: {e}"));
        let next = w.next_seq();
        if let Some(last) = last_reported {
            assert!(
                next > last,
                "iteration {i}: committed record {last} lost (next_seq {next})"
            );
        }
        for action in &w.recovery_report().actions {
            torn_zeroed += u32::from(matches!(action, RecoveryAction::ZeroedUncommitted { .. }));
            assert!(
                !matches!(
                    action,
                    RecoveryAction::RepairedTail { .. } | RecoveryAction::QuarantinedSegment { .. }
                ),
                "iteration {i}: a process kill must never look like corruption: {action:?}"
            );
        }
        drop(w);

        // Both live readers: the safe one and a zero-copy one (ADR 0027), which validates straight from the mapping.
        for (kind, slot) in [("safe", &mut reader), ("mapped", &mut mapped)] {
            let r = match slot {
                Some(r) => r,
                None => slot.insert(if kind == "mapped" {
                    // SAFETY: only the tickertape writer (killed and recovered here) modifies the journal, and it
                    // never modifies committed bytes, including during recovery.
                    unsafe { Reader::open_mapped(&journal, ReaderConfig::default()) }.unwrap()
                } else {
                    Reader::open(&journal, ReaderConfig::default()).unwrap()
                }),
            };
            while r.position() < next {
                let rec = r
                    .next_owned()
                    .unwrap_or_else(|e| {
                        panic!(
                            "iteration {i}: live {kind} reader error at {}: {e}",
                            r.position()
                        )
                    })
                    .unwrap_or_else(|| {
                        panic!("iteration {i}: {kind} record {} missing", r.position())
                    });
                assert_eq!(rec.schema_id, (rec.seq % 7) as u32);
                assert_eq!(rec.venue_ts_ns, rec.seq * 10);
                assert_eq!(
                    rec.payload,
                    payload(rec.seq, kill_len(rec.seq)),
                    "iteration {i}, {kind} reader, seq {}",
                    rec.seq
                );
            }
            assert!(
                r.try_next().unwrap().is_none(),
                "iteration {i}: {kind} reader sees more than recovery"
            );
        }
    }

    let all = read_all(&journal);
    for (i, rec) in all.iter().enumerate() {
        assert_eq!(rec.seq, i as u64);
    }
    eprintln!(
        "kill_loop: {iterations} kills ({killed_mid_run} after appends started, {torn_zeroed} torn appends zeroed), {} records, {} segments",
        all.len(),
        segments(&journal).len()
    );
    assert!(
        killed_mid_run > iterations / 4,
        "too few kills landed while appending"
    );
    if slow_fill {
        assert!(
            torn_zeroed > iterations / 4,
            "too few kills landed inside a fill"
        );
    }
}

#[test]
fn kill_loop_short() {
    kill_loop(20, false);
}

/// The M4 exit criterion: at least 200 kills. Run with `cargo test -p tickertape-tests --release -- --ignored`.
#[test]
#[ignore = "long: 250 kills; run with --release -- --ignored"]
fn kill_loop_250() {
    kill_loop(250, false);
}

/// ADR 0035: kills that land inside the fill closure (between the header fields and the commit word) leave a torn
/// append, which recovery zeroes in place and never mistakes for committed records hidden by a damaged commit word.
#[test]
fn kill_inside_fill() {
    kill_loop(30, true);
}

/// A zero-copy reader validating records **while** the writer fills and publishes them (ADR 0027): a thread polls
/// without sleeping while writer processes append records of mixed sizes (up to 3,199 bytes, so every CRC path
/// including the three-lane loop runs on live data), roll 64 KiB segments, and are killed and restarted. The reader
/// must never report an error and must return exactly the records each writer published, in order.
fn live_mapped_reader(iterations: u32) {
    const CATCH_UP: Duration = Duration::from_secs(60);
    let mut rng = Rng::from_env("live_mapped_reader");
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal");
    // The sequence number the reader stops at (set once the last writer is gone), and how far it has got.
    let stop_at = Arc::new(AtomicU64::new(u64::MAX));
    let progress = Arc::new(AtomicU64::new(0));
    let reader = {
        let (journal, stop_at, progress) = (journal.clone(), stop_at.clone(), progress.clone());
        std::thread::spawn(move || {
            let mut reader = loop {
                // SAFETY: only tickertape writers (killed and recovered here) modify the journal, and they never
                // modify committed bytes, including during recovery.
                match unsafe { Reader::open_mapped(&journal, ReaderConfig::default()) } {
                    Ok(reader) => break reader,
                    Err(_) if stop_at.load(Ordering::SeqCst) == u64::MAX => {
                        std::thread::yield_now()
                    }
                    Err(e) => panic!("open: {e}"),
                }
            };
            let mut empty_polls = 0_u64;
            // When the reader first saw `stop_at` set: it must then reach it within `CATCH_UP`, whether it is
            // polling an empty slot or still returning records, or the test fails instead of running (and the main
            // thread waiting in `join`) forever.
            let mut catching_up_since: Option<Instant> = None;
            loop {
                let expected = reader.position();
                match reader.try_next() {
                    Ok(Some(rec)) => {
                        assert_eq!(rec.seq, expected);
                        assert_eq!(rec.schema_id, (rec.seq % 7) as u32);
                        assert_eq!(rec.venue_ts_ns, rec.seq * 10);
                        assert_eq!(rec.local_ts_ns, rec.seq * 10 + 1);
                        assert!(
                            rec.payload == payload(rec.seq, mixed_len(rec.seq)),
                            "payload of {}",
                            rec.seq
                        );
                        progress.store(rec.seq + 1, Ordering::SeqCst);
                    }
                    Ok(None) => {
                        if reader.position() >= stop_at.load(Ordering::SeqCst) {
                            return empty_polls;
                        }
                        empty_polls += 1;
                        std::hint::spin_loop();
                    }
                    Err(e) => panic!("live mapped reader at {expected}: {e}"),
                }
                let stop = stop_at.load(Ordering::SeqCst);
                if stop != u64::MAX {
                    let since = *catching_up_since.get_or_insert_with(Instant::now);
                    assert!(
                        since.elapsed() < CATCH_UP,
                        "live mapped reader at position {} {CATCH_UP:?} after the last writer; expected to \
                         reach position {stop} (the final recovered next_seq)",
                        reader.position()
                    );
                }
            }
        })
    };

    let mut read_while_live = 0;
    for i in 0..iterations {
        let before = progress.load(Ordering::SeqCst);
        let mut child = Command::new(KILL_WRITER)
            .args([
                journal.to_str().unwrap(),
                &SEGMENT.to_string(),
                &rng.next_u64().to_string(),
                "mixed",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn writer");
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        if i % 3 == 2 {
            // Sometimes during open and recovery.
            std::thread::sleep(Duration::from_micros(rng.below(10_000)));
        } else {
            let mut line = String::new();
            stdout.read_line(&mut line).unwrap();
            std::thread::sleep(Duration::from_micros(rng.below(20_000)));
        }
        child.kill().expect("SIGKILL");
        child.wait().unwrap();
        if progress.load(Ordering::SeqCst) > before {
            read_while_live += 1;
        }
    }
    // Recover once more, then let the reader catch up to exactly what recovery kept.
    let next = Writer::open(&journal, WriterConfig::default().segment_size(SEGMENT))
        .expect("recovery")
        .next_seq();
    stop_at.store(next, Ordering::SeqCst);
    let empty_polls = reader.join().expect("live mapped reader failed");
    assert_eq!(progress.load(Ordering::SeqCst), next);
    let segs = segments(&journal).len();
    eprintln!(
        "live_mapped_reader: {iterations} kills, {next} records, {segs} segments, \
         {read_while_live} writers read while live, {empty_polls} empty polls"
    );
    assert!(segs >= 4, "only {segs} segments: too few rolls");
    assert!(
        read_while_live > iterations / 3,
        "the reader kept up with only {read_while_live} of {iterations} writers"
    );
    assert!(
        empty_polls > 0,
        "the reader never waited at an unpublished slot"
    );
}

#[test]
fn live_mapped_reader_short() {
    live_mapped_reader(12);
}

#[test]
#[ignore = "long: 120 kills; run with --release -- --ignored"]
fn live_mapped_reader_120() {
    live_mapped_reader(120);
}

/// A writer whose test is killed (a time-out, an interrupt) must not go on appending until the disk is full: it exits
/// once the process that started it is gone.
#[test]
fn an_orphaned_writer_exits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = dir.path().join("j");
    let script = format!(
        "\"{KILL_WRITER}\" \"{}\" 65536 1 > /dev/null 2>&1 & echo $!; sleep 1",
        journal.display()
    );
    let output = Command::new("sh")
        .args(["-c", &script])
        .output()
        .expect("sh");
    let pid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    assert!(pid.parse::<u32>().is_ok(), "{pid:?}");
    let alive = || {
        Command::new("kill")
            .args(["-0", &pid])
            .stderr(Stdio::null())
            .status()
            .expect("kill -0")
            .success()
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while alive() {
        if Instant::now() > deadline {
            let _ = Command::new("kill").args(["-9", &pid]).status();
            panic!("the orphaned writer {pid} was still running after 20 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
