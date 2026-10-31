//! Crash contract C1, C2, C3, C4: kill the writer process with SIGKILL at random points (including during recovery
//! and during segment rolls), then reopen and verify, many times over.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::time::Duration;

use tickertape::{Reader, ReaderConfig, RecoveryAction, Writer, WriterConfig};
use tickertape_tests::{Rng, kill_len, payload, read_all, segments};

const KILL_WRITER: &str = env!("CARGO_BIN_EXE_tt-kill-writer");
const SEGMENT: u64 = 64 * 1024;

fn kill_loop(iterations: u32) {
    let mut rng = Rng::from_env("kill_loop");
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal");
    // A live reader that stays open across every crash: it must never see a gap, corruption, or a discarded segment.
    let mut reader: Option<Reader> = None;
    let mut killed_mid_run = 0;

    for i in 0..iterations {
        let mut child = Command::new(KILL_WRITER)
            .args([
                journal.to_str().unwrap(),
                &SEGMENT.to_string(),
                &rng.next_u64().to_string(),
            ])
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
            assert!(
                !matches!(
                    action,
                    RecoveryAction::RepairedTail { .. } | RecoveryAction::QuarantinedSegment { .. }
                ),
                "iteration {i}: a process kill must never look like corruption: {action:?}"
            );
        }
        drop(w);

        let r = match &mut reader {
            Some(r) => r,
            None => reader.insert(Reader::open(&journal, ReaderConfig::default()).unwrap()),
        };
        while r.position() < next {
            let rec = r
                .next_owned()
                .unwrap_or_else(|e| {
                    panic!("iteration {i}: live reader error at {}: {e}", r.position())
                })
                .unwrap_or_else(|| panic!("iteration {i}: record {} missing", r.position()));
            assert_eq!(rec.schema_id, (rec.seq % 7) as u32);
            assert_eq!(rec.venue_ts_ns, rec.seq * 10);
            assert_eq!(
                rec.payload,
                payload(rec.seq, kill_len(rec.seq)),
                "iteration {i}, seq {}",
                rec.seq
            );
        }
        assert!(
            r.try_next().unwrap().is_none(),
            "iteration {i}: reader sees more than recovery"
        );
    }

    let all = read_all(&journal);
    for (i, rec) in all.iter().enumerate() {
        assert_eq!(rec.seq, i as u64);
    }
    eprintln!(
        "kill_loop: {iterations} kills ({killed_mid_run} after appends started), {} records, {} segments",
        all.len(),
        segments(&journal).len()
    );
    assert!(
        killed_mid_run > iterations / 4,
        "too few kills landed while appending"
    );
}

#[test]
fn kill_loop_short() {
    kill_loop(20);
}

/// The M4 exit criterion: at least 200 kills. Run with `cargo test -p tickertape-tests --release -- --ignored`.
#[test]
#[ignore = "long: 250 kills; run with --release -- --ignored"]
fn kill_loop_250() {
    kill_loop(250);
}
