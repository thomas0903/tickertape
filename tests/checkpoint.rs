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

/// The OS error behind an I/O error, looking through the wrapper that keeps it from counting as storage-full.
fn os_code(source: &std::io::Error) -> Option<i32> {
    source.raw_os_error().or_else(|| {
        source
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<std::io::Error>())
            .and_then(std::io::Error::raw_os_error)
    })
}

/// How a test gives up on a record inside its fill closure.
#[derive(Clone, Copy, Debug)]
enum Abandon {
    /// `append_with`'s closure panics.
    Panic,
    /// `try_append_with`'s closure returns `Err`.
    Decline,
}

#[test]
fn failed_cleanup_of_an_abandoned_write_is_reported() {
    failed_cleanup_is_reported(5 /* EIO */, Abandon::Panic);
}

#[test]
fn failed_cleanup_on_a_full_volume_is_not_storage_full() {
    // `fsync` can fail with ENOSPC on a full APFS volume (docs/crash-contract.md). Freeing space does not make this
    // writer usable again, so neither the recorded error nor a refused append may say "storage full, retry".
    failed_cleanup_is_reported(28 /* ENOSPC */, Abandon::Panic);
}

#[test]
fn failed_cleanup_of_a_declined_write_is_reported() {
    failed_cleanup_is_reported(5 /* EIO */, Abandon::Decline);
}

#[test]
fn failed_cleanup_of_a_declined_write_on_a_full_volume_is_not_storage_full() {
    failed_cleanup_is_reported(28 /* ENOSPC */, Abandon::Decline);
}

fn failed_cleanup_is_reported(errno: i32, how: Abandon) {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Writer::open(dir.path(), small_config()).unwrap();
    append_n(&mut w, 3);
    let record = tickertape::NewRecord {
        schema_id: 1,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload: &[],
    };
    fault::fail_next(Point::AbandonedWriteSync, errno);
    match how {
        Abandon::Panic => {
            let abandoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                w.append_with(&record, 16, |_| std::panic::panic_any("abandon"))
            }));
            assert!(abandoned.is_err());
        }
        Abandon::Decline => {
            // The decline is still reported as such; the failed clean-up shows in `abandoned_write_error`.
            let declined = w.try_append_with(&record, 16, |_| Err("abandon"));
            assert!(matches!(declined, Ok(Err("abandon"))), "{declined:?}");
        }
    }
    let cause = w
        .abandoned_write_error()
        .expect("the failed clean-up is recorded");
    assert!(
        matches!(cause, Error::Io { source, .. } if os_code(source) == Some(errno)),
        "{cause:?}"
    );
    assert!(!cause.is_storage_full(), "{cause:?}");
    // Later appends say why they are refused and carry the OS error, but are never storage-full: retrying after
    // freeing space would not help. Checkpoints say durability is lost.
    let refused = w
        .try_append_with(&record, 16, |_| -> Result<(), ()> {
            panic!("fill must not be called on a stopped writer")
        })
        .unwrap_err();
    assert!(!refused.is_storage_full(), "{refused:?}");
    for _ in 0..3 {
        let refused = w.append(&record).unwrap_err();
        assert!(!refused.is_storage_full(), "{refused:?}");
        match refused {
            Error::Io { op, source, .. } => {
                assert!(op.contains("abandoned write"), "{op}");
                assert_eq!(os_code(&source), Some(errno));
                assert_ne!(source.kind(), std::io::ErrorKind::StorageFull);
            }
            other => panic!("{other:?}"),
        }
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
