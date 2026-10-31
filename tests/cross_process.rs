//! M3: one writer process and several reader processes on the same journal.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tickertape::{Reader, ReaderConfig};
use tickertape_tests::{bounded_len, digest_step, payload, segments};

const WRITER: &str = env!("CARGO_BIN_EXE_tt-writer");
const READER: &str = env!("CARGO_BIN_EXE_tt-reader");

fn spawn_reader(dir: &Path, count: u64, max_payload: usize, mode: &str) -> Child {
    Command::new(READER)
        .args([
            dir.to_str().unwrap(),
            &count.to_string(),
            &max_payload.to_string(),
            mode,
        ])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn reader")
}

fn finish(child: Child, who: &str) -> String {
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "{who} failed with {}", out.status);
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn expected_digest(count: u64, max_payload: usize) -> String {
    let mut d = 0;
    for seq in 0..count {
        d = digest_step(
            d,
            seq,
            (seq % 7) as u32,
            &payload(seq, bounded_len(seq, max_payload)),
        );
    }
    format!("{count} {d:016x}")
}

/// Waits until a reader in this process has seen record `seq`.
fn wait_for_seq(dir: &Path, seq: u64) {
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut reader = loop {
        match Reader::open(dir, ReaderConfig::default()) {
            Ok(r) => break r,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => panic!("open: {e}"),
        }
    };
    reader.seek(seq).unwrap();
    assert!(
        reader
            .next_timeout(Some(Duration::from_secs(120)))
            .unwrap()
            .is_some(),
        "writer never reached {seq}"
    );
}

/// One writer, two live readers (safe and zero-copy) from the start, and a late reader that starts when the writer
/// is halfway (and paused) and must replay from 0 and catch up. All must see the identical gap-free sequence.
fn run(count: u64, segment_size: u64, max_payload: usize) {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("journal");
    let go = dir.path().join("go");
    let half = count / 2;

    let early_safe = spawn_reader(&journal, count, max_payload, "safe");
    let early_mapped = spawn_reader(&journal, count, max_payload, "mapped");
    let writer = Command::new(WRITER)
        .args([
            journal.to_str().unwrap(),
            &count.to_string(),
            &segment_size.to_string(),
            &max_payload.to_string(),
            &half.to_string(),
            go.to_str().unwrap(),
        ])
        .spawn()
        .expect("spawn writer");

    wait_for_seq(&journal, half - 1);
    let late = spawn_reader(&journal, count, max_payload, "safe");
    std::fs::write(&go, b"").unwrap();

    let writer_out = writer.wait_with_output().unwrap();
    assert!(writer_out.status.success(), "writer failed");
    let expected = expected_digest(count, max_payload);
    assert_eq!(finish(early_safe, "early safe reader"), expected);
    assert_eq!(finish(early_mapped, "early mapped reader"), expected);
    assert_eq!(finish(late, "late reader"), expected);
    let rolls = segments(&journal).len();
    assert!(
        rolls >= 3,
        "expected several segment rolls, got {rolls} segments"
    );
}

#[test]
fn two_readers_and_a_late_reader_small() {
    run(50_000, 64 * 1024, 64);
}

/// The M3 exit criterion: at least 1,000,000 records across segment rolls. Run with
/// `cargo test -p tickertape-tests --release -- --ignored`.
#[test]
#[ignore = "long: 1M records; run with --release -- --ignored"]
fn two_readers_and_a_late_reader_1m() {
    run(1_000_000, 1024 * 1024, 64);
}
