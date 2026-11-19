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

#[test]
fn failed_cleanup_of_an_abandoned_write_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 3);
    let record = tickertape::NewRecord {
        schema_id: 1,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload: &[],
    };
    fault::fail_next(Point::AbandonedWriteSync, 5 /* EIO */);
    let abandoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        w.append_with(&record, 16, |_| std::panic::panic_any("abandon"))
    }));
    assert!(abandoned.is_err());
    let cause = w
        .abandoned_write_error()
        .expect("the failed clean-up is recorded");
    assert!(
        matches!(cause, Error::Io { source, .. } if source.raw_os_error() == Some(5)),
        "{cause:?}"
    );
    // Later appends say why they are refused and carry the OS error; checkpoints say durability is lost.
    match w.append(&record) {
        Err(Error::Io { op, source, .. }) => {
            assert!(op.contains("abandoned write"), "{op}");
            assert_eq!(source.raw_os_error(), Some(5));
        }
        other => panic!("{other:?}"),
    }
    let lost = w.checkpoint().unwrap_err();
    assert!(matches!(lost, Error::DurabilityLost));
    assert!(lost.to_string().contains("abandoned write"), "{lost}");
    drop(w);
    // A new writer's recovery cleans up and starts without the error.
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    assert!(w.abandoned_write_error().is_none());
    append_n(&mut w, 1);
    w.checkpoint().unwrap();
    assert_eq!(read_all(dir.path()).len(), 4);
}
