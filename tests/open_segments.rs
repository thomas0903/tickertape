//! The writer holds a bounded number of open segments between checkpoints (ADR 0038 → Bounded open segments): a
//! writer that rolls many times without a checkpoint, with background checkpoints, or whose recovery scans many
//! segments, runs with a descriptor limit far below the number of segments it covers; a checkpoint reopens the
//! segments it released, and fails (poisoning the writer) if one of them is gone.

use std::path::Path;
use std::process::Command;

use tickertape::{Error, Reader, ReaderConfig, Writer};
use tickertape_tests::{append_n, check_record, segments, small_config};

const HELPER: &str = env!("CARGO_BIN_EXE_tt-open-segments");
/// The child's descriptor limit, far below the number of segments in each test.
const LIMIT: u32 = 64;
const ROLLS: u64 = 300;

/// Runs the helper with `RLIMIT_NOFILE` lowered to [`LIMIT`]; returns the numbers it printed.
fn run_limited(args: &[&str]) -> Vec<i64> {
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("ulimit -n {LIMIT} && exec \"$0\" \"$@\""))
        .arg(HELPER)
        .args(args)
        .output()
        .expect("spawn helper");
    assert!(
        out.status.success(),
        "helper {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.parse().unwrap())
        .collect()
}

/// Reads the journal with a durable-only reader and checks it holds exactly records `0..next_seq`.
fn check_durable(dir: &Path, next_seq: u64) {
    let mut reader = Reader::open(dir, ReaderConfig::default().durable_only(true)).unwrap();
    let mut seq = 0;
    while let Some(record) = reader.next_owned().unwrap() {
        assert_eq!(record.seq, seq);
        check_record(&record);
        seq += 1;
    }
    assert_eq!(seq, next_seq, "durable records");
}

/// Appends until `writer` has rolled `rolls` times.
fn roll(writer: &mut Writer, rolls: u64, checkpoint_every: u64) {
    let mut base = writer.metrics().head.segment_base_seq;
    let mut rolled = 0;
    while rolled < rolls {
        append_n(writer, 1);
        let now = writer.metrics().head.segment_base_seq;
        if now != base {
            base = now;
            rolled += 1;
            if checkpoint_every > 0 && rolled % checkpoint_every == 0 {
                writer.checkpoint().unwrap();
            }
        }
    }
}

fn rolls_under_a_low_limit(request_every: u64) {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let out = run_limited(&[
        "roll",
        journal.to_str().unwrap(),
        &ROLLS.to_string(),
        &request_every.to_string(),
    ]);
    let (next_seq, fds, maps) = (out[0] as u64, out[1], out[2]);
    assert!(segments(&journal).len() as u64 > ROLLS);
    // From the first roll to the last, the descriptors and mappings held stay within a few segments.
    assert!(fds <= 8, "descriptors grew by {fds} over {ROLLS} rolls");
    assert!(maps <= 16, "mappings grew by {maps} over {ROLLS} rolls");
    check_durable(&journal, next_seq);
}

#[test]
fn many_rolls_without_a_checkpoint_under_a_low_descriptor_limit() {
    rolls_under_a_low_limit(0);
}

#[test]
fn many_rolls_with_background_checkpoints_under_a_low_descriptor_limit() {
    rolls_under_a_low_limit(25);
}

/// Builds a journal of more than [`ROLLS`] segments (checkpointing every 50 rolls, so that building it holds few
/// descriptors whatever the writer does), then moves its marker away: the next writer's recovery scans every segment.
fn journal_without_marker(journal: &Path) -> u64 {
    let mut writer = Writer::open(journal, small_config()).unwrap();
    roll(&mut writer, ROLLS, 50);
    writer.checkpoint().unwrap();
    let next_seq = writer.next_seq();
    drop(writer);
    std::fs::remove_file(journal.join("checkpoint")).unwrap();
    next_seq
}

fn open_without_marker_under_a_low_limit(policy: &str) {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let before = journal_without_marker(&journal);
    let out = run_limited(&["open", journal.to_str().unwrap(), policy]);
    assert_eq!(out[0] as u64, before + 1);
    check_durable(&journal, before + 1);
}

#[test]
fn open_without_a_marker_scans_more_segments_than_the_limit_fail() {
    open_without_marker_under_a_low_limit("fail");
}

#[test]
fn open_without_a_marker_scans_more_segments_than_the_limit_quarantine() {
    open_without_marker_under_a_low_limit("quarantine");
}

/// A released segment that is gone at the checkpoint (removed by hand) fails it as an I/O error and poisons the
/// writer, with a blocking or a background checkpoint.
fn deleted_released_segment(background: bool) {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("j");
    let mut writer = Writer::open(&journal, small_config()).unwrap();
    roll(&mut writer, 12, 0);
    // The second segment: far outside the few the writer keeps open, and after the (absent) marker.
    let (_, path) = segments(&journal)[1].clone();
    std::fs::remove_file(&path).unwrap();
    let first = if background {
        writer.request_checkpoint().unwrap();
        writer.checkpoint()
    } else {
        writer.checkpoint()
    };
    match first {
        Err(Error::Io { path: failed, .. }) => assert_eq!(failed, path),
        other => panic!(
            "expected an I/O error for {}, got {other:?}",
            path.display()
        ),
    }
    assert!(matches!(writer.checkpoint(), Err(Error::DurabilityLost)));
    assert!(writer.metrics().poisoned);
    // Appends keep working.
    append_n(&mut writer, 1);
}

#[test]
fn a_deleted_released_segment_fails_the_checkpoint_and_poisons() {
    deleted_released_segment(false);
}

#[test]
fn a_deleted_released_segment_fails_a_background_checkpoint_and_poisons() {
    deleted_released_segment(true);
}
