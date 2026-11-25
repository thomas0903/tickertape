//! A `loom` model of the histogram itself (not a copy of it): one thread records, another takes snapshots.
//!
//! Run with the publication protocol's model (the name matches its filter):
//! `RUSTFLAGS="--cfg loom" cargo test -p tickertape --lib --release protocol_model`
//!
//! The recording thread is the model's first one, and the snapshots are taken in the spawned one. Loom runs the first
//! thread for as long as it can, and looks for other interleavings only where an operation depends on the **last**
//! operation on the same atomic. The recorder loads each counter before it stores it, so in the other arrangement its
//! own load hides the snapshot's from loom, and loom explores a single interleaving.

use super::histogram::Histogram;
use loom::sync::Arc;
use loom::thread;
use std::time::Duration;

/// Checks what [`Snapshot`](super::Snapshot) documents of one that was taken at any time, and returns its count.
fn check(s: &super::Snapshot) -> u64 {
    let mut sum = 0;
    for bucket in s.buckets() {
        assert!(s.max() >= Some(Duration::from_nanos(bucket.high_ns)));
        assert!(s.min() <= Some(Duration::from_nanos(bucket.low_ns)));
        sum += bucket.low_ns * bucket.count;
    }
    assert!(s.sum() >= Some(Duration::from_nanos(sum)));
    s.count()
}

/// In every interleaving: a snapshot's extremes cover every value it counts, its sum includes them, a later snapshot
/// counts no less, and none is lost. Both extremes move.
#[test]
fn snapshots_cover_what_they_count() {
    let partial = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let seen = partial.clone();
    loom::model(move || {
        let histogram = Arc::new(Histogram::with_buckets(3));
        let reading = {
            let histogram = histogram.clone();
            thread::spawn(move || {
                let first = histogram.snapshot();
                (check(&first), first)
            })
        };
        histogram.record(1);
        histogram.record(2);
        histogram.record(0);
        let (counted, first) = reading.join().unwrap();
        seen.fetch_or(1 << counted, std::sync::atomic::Ordering::Relaxed);
        let last = histogram.snapshot();
        assert_eq!(check(&last), 3);
        assert_eq!(last.sum(), Some(Duration::from_nanos(3)));
        assert_eq!(last.min(), Some(Duration::ZERO));
        assert_eq!(last.max(), Some(Duration::from_nanos(2)));
        let rest = last.since(&first);
        assert_eq!(rest.count() + counted, 3);
        for bucket in rest.buckets() {
            assert!(rest.max() >= Some(Duration::from_nanos(bucket.high_ns)));
            assert!(rest.min() <= Some(Duration::from_nanos(bucket.low_ns)));
        }
    });
    // The model explored snapshots of nothing, of one and two values, and of all three.
    assert_eq!(partial.load(std::sync::atomic::Ordering::Relaxed), 0b1111);
}
