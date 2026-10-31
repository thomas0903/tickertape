//! Crash contract C12: checkpoint errors are sticky.

use tickertape::fault::{self, Point};
use tickertape::{Error, Writer};
use tickertape_tests::{append_n, read_all, small_config};

#[test]
fn poisoned_after_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 100);
    w.checkpoint().unwrap();
    append_n(&mut w, 100);

    fault::fail_next(Point::CheckpointSync, 5 /* EIO */);
    assert!(matches!(w.checkpoint(), Err(Error::Io { .. })));
    // A retry must not claim durability, even though the injected failure is gone.
    assert!(matches!(w.checkpoint(), Err(Error::DurabilityLost)));
    assert!(matches!(w.checkpoint(), Err(Error::DurabilityLost)));
    // Visibility is unaffected.
    append_n(&mut w, 10);
    assert_eq!(read_all(dir.path()).len(), 210);
    drop(w);

    // A new writer starts unpoisoned (the crash contract documents what that does and doesn't mean).
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    assert_eq!(
        w.recovery_report().scanned_from_seq,
        100,
        "marker from the last good checkpoint"
    );
    w.checkpoint().unwrap();
}

#[test]
fn checkpoint_advances_marker() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    for round in 1..=5u64 {
        append_n(&mut w, 333);
        w.checkpoint().unwrap();
        // Checkpointing twice with nothing new is fine.
        w.checkpoint().unwrap();
        drop(w);
        w = Writer::open(dir.path(), small_config()).unwrap();
        assert!(w.recovery_report().had_checkpoint);
        assert_eq!(w.recovery_report().scanned_from_seq, round * 333);
        assert!(w.recovery_report().actions.is_empty());
    }
}
