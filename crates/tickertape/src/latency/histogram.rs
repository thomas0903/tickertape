//! The histogram behind [`Recorder`](super::Recorder): a log-linear layout over all of `u64`, one recording thread,
//! and snapshots from any thread (ADR 0047).

#[cfg(all(test, loom))]
use loom::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(all(test, loom)))]
use std::sync::atomic::{AtomicU64, Ordering};

use std::fmt;
use std::time::Duration;

/// Values below `2 * SUB` have a bucket each; from there on, every power of two has `SUB` equal buckets.
const SUB: u64 = 64;
/// The number of buckets: the last one ends at `u64::MAX`.
const BUCKETS: usize = 3776;

/// The bucket that holds `value`.
#[inline]
fn index(value: u64) -> usize {
    let shift = 57u32.saturating_sub((value | 1).leading_zeros());
    // At most 64 * 57 + 127 = 3775, so the cast loses nothing on a 32-bit target either.
    (SUB * u64::from(shift) + (value >> shift)) as usize
}

/// The smallest and the largest value that bucket `i` (below [`BUCKETS`]) holds.
fn bounds(i: usize) -> (u64, u64) {
    let i = i as u64;
    if i < 2 * SUB {
        return (i, i);
    }
    let shift = i / SUB - 1;
    let low = (i % SUB + SUB) << shift;
    // Not `low + (1 << shift) - 1`: in the last bucket `low + (1 << shift)` would overflow.
    (low, low | ((1 << shift) - 1))
}

/// The counters. One thread calls [`Histogram::record`] (the caller's exclusive borrow of the recorder sees to it);
/// any thread calls [`Histogram::snapshot`].
pub(super) struct Histogram {
    /// `u64::MAX` until the first value.
    min: AtomicU64,
    max: AtomicU64,
    /// Saturating: `u64::MAX` means unknown from here on.
    sum: AtomicU64,
    buckets: Box<[AtomicU64]>,
}

impl Default for Histogram {
    fn default() -> Histogram {
        Histogram::with_buckets(BUCKETS)
    }
}

impl Histogram {
    /// `n` is [`BUCKETS`], except in the loom model, which has few so that it stays small.
    pub(super) fn with_buckets(n: usize) -> Histogram {
        let buckets: Box<[AtomicU64]> = (0..n).map(|_| AtomicU64::new(0)).collect();
        // Written once more, with atomic stores, so that the pages are there before the first value is recorded: the
        // compiler may turn the allocation above into one of lazily zeroed pages.
        for bucket in &buckets {
            bucket.store(0, Ordering::Relaxed);
        }
        Histogram {
            min: AtomicU64::new(u64::MAX),
            max: AtomicU64::new(0),
            sum: AtomicU64::new(0),
            buckets,
        }
    }

    /// Counts one value. Loads and stores without a locked instruction, which is right only because a single thread at
    /// a time records.
    ///
    /// The bucket comes last, with `Release`: a snapshot that counts this value (an `Acquire` load of the bucket) also
    /// sees the extremes and the sum that include it.
    #[inline]
    pub(super) fn record(&self, value: u64) {
        let Some(bucket) = self.buckets.get(index(value)) else {
            return; // `index` is below `BUCKETS`.
        };
        if value < self.min.load(Ordering::Relaxed) {
            self.min.store(value, Ordering::Relaxed);
        }
        if value > self.max.load(Ordering::Relaxed) {
            self.max.store(value, Ordering::Relaxed);
        }
        let sum = self.sum.load(Ordering::Relaxed).saturating_add(value);
        self.sum.store(sum, Ordering::Relaxed);
        let count = bucket.load(Ordering::Relaxed).wrapping_add(1);
        bucket.store(count, Ordering::Release);
    }

    /// Copies every bucket, then the sum and the extremes.
    pub(super) fn snapshot(&self) -> Snapshot {
        let buckets: Box<[u64]> = self
            .buckets
            .iter()
            .map(|bucket| bucket.load(Ordering::Acquire))
            .collect();
        Snapshot {
            count: total(&buckets),
            buckets,
            sum: self.sum.load(Ordering::Relaxed),
            min: self.min.load(Ordering::Relaxed),
            max: self.max.load(Ordering::Relaxed),
        }
    }
}

fn total(buckets: &[u64]) -> u64 {
    buckets.iter().fold(0, |n, &b| n.saturating_add(b))
}

/// A copy of a histogram at one moment, or of an interval ([`Snapshot::since`]).
///
/// The count is the sum of the buckets, so the quantiles always fit it. If values were being recorded while the
/// snapshot was taken, then
///
/// - the maximum is never below, and the minimum never above, any value that is counted;
/// - the sum (hence the mean) and the extremes can already include any number of values that are not counted: of those
///   that were recorded while the snapshot was taken. The sum is never below that of the values counted;
/// - the counted values are not a prefix of the recorded ones: the n-th can be missing and the one after it counted.
///
/// With a count of 0, everything is reported as empty. Counts of later snapshots are never smaller.
#[derive(Clone)]
pub struct Snapshot {
    buckets: Box<[u64]>,
    /// The sum of `buckets`.
    count: u64,
    /// `u64::MAX`: saturated, so unknown.
    sum: u64,
    /// As recorded, meaningful with a `count` above 0: `u64::MAX` and 0 before the first value.
    min: u64,
    max: u64,
}

impl Snapshot {
    /// How many values are counted.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The sum of the values; zero when empty. `None` once it has reached `u64::MAX` nanoseconds (it saturates there),
    /// also for an interval that ends after that.
    ///
    /// Exact if nothing was recorded while the snapshot was taken. Otherwise it is too large by at most the values
    /// recorded meanwhile, any number of them, as [`Snapshot`] says. For an interval it can be off either way: too large
    /// by those of its end, too small by those of its start.
    pub fn sum(&self) -> Option<Duration> {
        match (self.count, self.sum) {
            (0, _) => Some(Duration::ZERO),
            (_, u64::MAX) => None,
            (_, sum) => Some(Duration::from_nanos(sum)),
        }
    }

    /// The mean of the values, rounded down to a nanosecond. `None` when empty, and when [`Snapshot::sum`] is `None`.
    ///
    /// It is [`Snapshot::sum`] divided by [`Snapshot::count`], so it is exact if nothing was recorded while the snapshot
    /// was taken. Otherwise the sum can include values that the count does not, and the mean is too large; for an
    /// interval it can be off **either way**. It can lie outside `[min, max]`. The error is that of a fixed number of
    /// values, those recorded during one snapshot (two for an interval), so it shrinks as the count grows: a short
    /// interval of a busy recorder can be far off, a long one hardly is. The quantiles do not have it.
    pub fn mean(&self) -> Option<Duration> {
        let sum = self.sum()?.as_nanos().checked_div(self.count.into())?;
        Some(Duration::from_nanos(u64::try_from(sum).unwrap_or(u64::MAX)))
    }

    /// The smallest value; `None` when empty. Exact for a snapshot. After [`Snapshot::since`] it may be a bucket bound,
    /// never above the interval's true minimum.
    pub fn min(&self) -> Option<Duration> {
        (self.count > 0).then(|| Duration::from_nanos(self.min))
    }

    /// The largest value; `None` when empty. Exact for a snapshot. After [`Snapshot::since`] it may be a bucket bound,
    /// never below the interval's true maximum.
    pub fn max(&self) -> Option<Duration> {
        (self.count > 0).then(|| Duration::from_nanos(self.max))
    }

    /// The value at quantile `q`, for example 0.999: with nearest rank, the value with the smallest rank `r` in
    /// `1..=count` for which `r / count >= q`, given as the upper bound of its bucket, capped at the maximum. So it is
    /// never below the true value, and at most 1/64 (1.6 %) above it. With few values the high quantiles
    /// are the maximum (0.9999 of 1,000 values has rank 1,000).
    ///
    /// `None` when empty, and when `q` is not in `0.0..=1.0`.
    pub fn value_at_quantile(&self, q: f64) -> Option<Duration> {
        let rank = rank(self.count, q)?;
        let mut seen = 0u64;
        for (i, &n) in self.buckets.iter().enumerate() {
            seen = seen.saturating_add(n);
            if seen >= rank {
                return Some(Duration::from_nanos(bounds(i).1.min(self.max)));
            }
        }
        None
    }

    /// The values recorded between `earlier`, a snapshot of the same histogram, and this one.
    ///
    /// The maximum of an interval is exact if the histogram's maximum rose in it; otherwise it is the upper bound of the
    /// interval's highest bucket, capped at the histogram's maximum. It is never too small, and at most 1/64 too large
    /// (the minimum likewise, never too large), except that it can be that of values that were recorded while this
    /// snapshot was taken and that the interval does not count. The sum and the mean can be off either way, as
    /// [`Snapshot::mean`] says.
    ///
    /// The subtraction saturates: with the two snapshots swapped, or of different histograms, nothing panics and the
    /// result means nothing.
    pub fn since(&self, earlier: &Snapshot) -> Snapshot {
        let buckets: Box<[u64]> = self
            .buckets
            .iter()
            .zip(&earlier.buckets)
            .map(|(later, earlier)| later.saturating_sub(*earlier))
            .collect();
        let lowest = buckets.iter().position(|&n| n > 0);
        let highest = buckets.iter().rposition(|&n| n > 0);
        let min = match lowest {
            Some(i) if self.min >= earlier.min => bounds(i).0.max(self.min),
            Some(_) => self.min,
            None => u64::MAX,
        };
        let max = match highest {
            Some(i) if self.max <= earlier.max => bounds(i).1.min(self.max),
            Some(_) => self.max,
            None => 0,
        };
        Snapshot {
            count: total(&buckets),
            buckets,
            sum: if self.sum == u64::MAX || earlier.sum == u64::MAX {
                u64::MAX
            } else {
                self.sum.saturating_sub(earlier.sum)
            },
            min,
            max,
        }
    }

    /// The buckets that hold at least one value, in ascending order. Which buckets there are may change between
    /// versions; their bounds say what each one holds.
    pub fn buckets(&self) -> impl Iterator<Item = Bucket> + '_ {
        self.buckets
            .iter()
            .enumerate()
            .filter(|(_, count)| **count > 0)
            .map(|(i, &count)| {
                let (low_ns, high_ns) = bounds(i);
                Bucket {
                    low_ns,
                    high_ns,
                    count,
                }
            })
    }
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("count", &self.count)
            .field("sum", &self.sum())
            .field("min", &self.min())
            .field("max", &self.max())
            .finish_non_exhaustive()
    }
}

/// The smallest `r` in `1..=count` with `r / count >= q`, compared as `f64` like the caller's `q`: 7 of 100 values reach
/// `0.07`, although `0.07 * 100.0` is above 7.
fn rank(count: u64, q: f64) -> Option<u64> {
    if count == 0 || !(0.0..=1.0).contains(&q) {
        return None;
    }
    let reaches = |r: u64| r as f64 / count as f64 >= q;
    let (mut low, mut high) = (1, count); // `count` reaches every `q`.
    while low < high {
        let middle = low + (high - low) / 2;
        if reaches(middle) {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    Some(high)
}

/// A bucket of a [`Snapshot`]: how many values lie between two bounds, both included.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Bucket {
    /// The smallest value the bucket holds, in nanoseconds.
    pub low_ns: u64,
    /// The largest value the bucket holds, in nanoseconds.
    pub high_ns: u64,
    /// How many values it holds.
    pub count: u64,
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::super::{Recorder, View};
    use super::*;

    fn recorded(values: &[u64]) -> Snapshot {
        let mut recorder = Recorder::new();
        for &v in values {
            recorder.record_ns(v);
        }
        recorder.view().snapshot()
    }

    fn ns(d: Option<Duration>) -> Option<u64> {
        d.map(|d| d.as_nanos() as u64)
    }

    #[test]
    fn buckets_are_contiguous_and_cover_all_of_u64() {
        let mut next = 0u64;
        for i in 0..BUCKETS {
            let (low, high) = bounds(i);
            assert_eq!(low, next, "bucket {i} does not start where the last ended");
            assert!(low <= high);
            assert_eq!(index(low), i);
            assert_eq!(index(high), i);
            assert_eq!(index(low + (high - low) / 2), i);
            if i >= 128 {
                assert!(high - low < low / 64 + 1, "bucket {i} is too wide");
                assert!((high - low + 1) * 64 <= low, "bucket {i} is too wide");
            } else {
                assert_eq!((low, high), (i as u64, i as u64));
            }
            match high.checked_add(1) {
                Some(after) => next = after,
                None => assert_eq!(i, BUCKETS - 1, "u64::MAX is in the last bucket only"),
            }
        }
        assert_eq!(bounds(BUCKETS - 1).1, u64::MAX);
    }

    #[test]
    fn empty() {
        let s = recorded(&[]);
        assert_eq!(s.count(), 0);
        assert_eq!(s.sum(), Some(Duration::ZERO));
        assert_eq!(s.mean(), None);
        assert_eq!(s.min(), None);
        assert_eq!(s.max(), None);
        assert_eq!(s.value_at_quantile(0.5), None);
        assert_eq!(s.buckets().count(), 0);
        let nothing = s.since(&s);
        assert_eq!((nothing.count(), nothing.max()), (0, None));
        assert_eq!(nothing.sum(), Some(Duration::ZERO));
    }

    #[test]
    fn the_ends_of_the_range() {
        let s = recorded(&[0, u64::MAX]);
        assert_eq!(s.count(), 2);
        assert_eq!(ns(s.min()), Some(0));
        assert_eq!(ns(s.max()), Some(u64::MAX));
        assert_eq!(ns(s.value_at_quantile(0.5)), Some(0));
        assert_eq!(ns(s.value_at_quantile(1.0)), Some(u64::MAX));
        // The sum reached u64::MAX, where it can't be told from a saturated one.
        assert_eq!(s.sum(), None);

        let mut recorder = Recorder::new();
        recorder.record(Duration::MAX);
        recorder.record(Duration::ZERO);
        let s = recorder.view().snapshot();
        assert_eq!(ns(s.max()), Some(u64::MAX));
        assert_eq!(ns(s.min()), Some(0));
    }

    #[test]
    fn a_saturated_sum_is_unknown_and_nothing_overflows() {
        // Uptime nanoseconds taken for Unix time: decades each, and the eleventh overflows a u64.
        let mut recorder = Recorder::new();
        let view = recorder.view();
        for _ in 0..10 {
            recorder.record_ns(1_800_000_000_000_000_000);
        }
        let ten = view.snapshot();
        assert_eq!(ns(ten.sum()), Some(18_000_000_000_000_000_000));
        assert_eq!(ns(ten.mean()), Some(1_800_000_000_000_000_000));
        recorder.record_ns(1_800_000_000_000_000_000);
        let eleven = view.snapshot();
        assert_eq!(eleven.count(), 11);
        assert_eq!(eleven.sum(), None);
        assert_eq!(eleven.mean(), None);
        assert_eq!(ns(eleven.max()), Some(1_800_000_000_000_000_000));
        let last = eleven.since(&ten);
        assert_eq!(last.count(), 1);
        assert_eq!(last.sum(), None);
        assert_eq!(last.mean(), None);
        // Once saturated, always: later intervals too, also when both ends are saturated.
        recorder.record_ns(5);
        let twelve = view.snapshot();
        assert_eq!(twelve.sum(), None);
        assert_eq!(twelve.since(&eleven).sum(), None);
        assert_eq!(twelve.since(&eleven).mean(), None);
        // No value at all in the interval: its sum is known.
        assert_eq!(twelve.since(&twelve).sum(), Some(Duration::ZERO));
    }

    #[test]
    fn mean_and_sum() {
        let s = recorded(&[10, 20, 33]);
        assert_eq!(ns(s.sum()), Some(63));
        assert_eq!(ns(s.mean()), Some(21));
    }

    #[test]
    fn quantiles_out_of_range_are_absent() {
        let s = recorded(&[1, 2, 3]);
        for q in [-0.1, 1.000_1, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(s.value_at_quantile(q), None, "{q}");
        }
        assert_eq!(ns(s.value_at_quantile(0.0)), Some(1));
        assert_eq!(ns(s.value_at_quantile(-0.0)), Some(1));
        assert_eq!(ns(s.value_at_quantile(1.0)), Some(3));
    }

    #[test]
    fn quantile_ranks_where_the_product_rounds_badly() {
        // 0.07 * 100.0 is 7.000000000000001 and 0.29 * 100.0 is 28.999999999999996.
        for (count, q, want) in [
            (100, 0.07, 7),
            (100, 0.29, 29),
            (100, 0.14, 14),
            (100, 0.28, 28),
            (100, 0.55, 55),
            (1000, 0.999, 999),
            (10_000, 0.9999, 9999),
            (1_000_000, 0.9999, 999_900),
            (3, 1.0 / 3.0, 1),
            (3, 2.0 / 3.0, 2),
            (10, 0.1, 1),
            (10, 0.100_000_000_1, 2),
            (1, 0.0, 1),
            (1, 1.0, 1),
            (u64::MAX, 1.0, u64::MAX - 1023), // the first rank whose f64 is 2^64
            (u64::MAX, 0.0, 1),
        ] {
            assert_eq!(rank(count, q), Some(want), "rank of {q} in {count}");
        }
        // Every k / n, for many n: the rank is k.
        let n_max = if cfg!(miri) { 20 } else { 400 };
        for n in 1..n_max {
            for k in 1..=n {
                assert_eq!(rank(n, k as f64 / n as f64), Some(k), "{k} / {n}");
            }
        }
    }

    #[test]
    fn extremes_of_an_interval() {
        let mut recorder = Recorder::new();
        let view = recorder.view();
        recorder.record_ns(1_000);
        recorder.record_ns(1_000_000);
        let first = view.snapshot();
        // Neither extreme moves: bucket bounds, on the safe side.
        recorder.record_ns(50_001);
        recorder.record_ns(60_001);
        let second = view.snapshot();
        let interval = second.since(&first);
        assert_eq!(interval.count(), 2);
        assert_eq!(ns(interval.sum()), Some(110_002));
        let (min, max) = (ns(interval.min()).unwrap(), ns(interval.max()).unwrap());
        assert!((49_220..=50_001).contains(&min), "{min}");
        assert!((60_001..=60_939).contains(&max), "{max}");
        // Both move: exact.
        recorder.record_ns(7);
        recorder.record_ns(2_000_003);
        let third = view.snapshot();
        let interval = third.since(&second);
        assert_eq!(ns(interval.min()), Some(7));
        assert_eq!(ns(interval.max()), Some(2_000_003));
        // The bound of the highest bucket, capped at the overall maximum.
        recorder.record_ns(2_000_003);
        let interval = view.snapshot().since(&third);
        assert_eq!(ns(interval.max()), Some(2_000_003));
        assert_eq!(ns(interval.value_at_quantile(0.5)), Some(2_000_003));
        // Nested: never too small.
        let nested = view.snapshot().since(&first).since(&second.since(&first));
        assert_eq!(nested.count(), 3);
        assert!(ns(nested.max()) >= Some(2_000_003));
        // Swapped, and snapshots of different histograms: no panic.
        let swapped = first.since(&third);
        assert_eq!((swapped.count(), swapped.max()), (0, None));
        let _ = recorded(&[5, 6]).since(&third);
        let _ = third.since(&recorded(&[u64::MAX; 3]));
    }

    #[test]
    fn views_outlive_the_recorder_and_debug_is_short() {
        let mut recorder = Recorder::new();
        let view = recorder.view();
        recorder.record_ns(42);
        assert!(format!("{recorder:?} {view:?}").len() < 100);
        drop(recorder);
        let s = view.clone().snapshot();
        assert_eq!(ns(s.max()), Some(42));
        assert!(format!("{s:?}").len() < 200);
        let buckets: Vec<_> = s
            .buckets()
            .map(|b| (b.low_ns, b.high_ns, b.count))
            .collect();
        assert_eq!(buckets, [(42, 42, 1)]);
    }

    #[test]
    fn types_cross_threads_as_documented() {
        fn send<T: Send>() {}
        fn sync<T: Sync>() {}
        send::<Recorder>();
        send::<View>();
        sync::<View>();
        send::<Snapshot>();
    }

    /// While one thread records, snapshots of another never go backwards, never miss the extremes of what they count,
    /// and the last one holds everything. Run in release mode too (CI's long tests and any `--release` run).
    #[test]
    fn concurrent_snapshots() {
        const VALUES: u64 = if cfg!(miri) { 60 } else { 2_000_000 };
        // Values go up, so that the maximum moves all the time.
        let value = |i: u64| 10 + i * 7;
        let mut recorder = Recorder::new();
        let view = recorder.view();
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let reading = scope.spawn(|| {
                let mut previous = view.snapshot();
                let mut taken = 0;
                loop {
                    let last = done.load(std::sync::atomic::Ordering::Acquire);
                    let s = view.snapshot();
                    for (later, earlier) in s.buckets.iter().zip(&previous.buckets) {
                        assert!(later >= earlier);
                    }
                    if let Some(highest) = s.buckets().last() {
                        assert!(s.max >= highest.low_ns, "{s:?}");
                        assert_eq!(s.min, 10);
                    }
                    let interval = s.since(&previous);
                    if let Some(highest) = interval.buckets().last() {
                        assert!(interval.max >= highest.low_ns, "{interval:?}");
                    }
                    assert_eq!(s.count, total(&s.buckets));
                    previous = s;
                    taken += 1;
                    if last {
                        return (previous, taken);
                    }
                }
            });
            for i in 0..VALUES {
                recorder.record_ns(value(i));
            }
            done.store(true, std::sync::atomic::Ordering::Release);
            let (last, taken) = reading.join().unwrap();
            assert!(taken > 0);
            assert_eq!(last.count(), VALUES);
            assert_eq!(ns(last.max()), Some(value(VALUES - 1)));
            assert_eq!(ns(last.sum()), Some((0..VALUES).map(value).sum()));
        });
    }

    /// The documented bound on the sum, with the recorder never idle: it is at least that of the values counted, and
    /// exceeds it by at most the values recorded while the snapshot was taken; for an interval, by those of its two
    /// snapshots either way. All values are equal, so what the sum misses or has too much is a number of values.
    #[test]
    #[cfg_attr(miri, ignore = "minutes; loom and the test above check the orderings")]
    fn sums_are_off_by_at_most_what_was_recorded_during_the_snapshot() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};
        struct StopOnDrop<'a>(&'a AtomicBool);
        impl Drop for StopOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        const VALUE: u64 = 100;
        let snapshots = 2_000;
        let mut recorder = Recorder::new();
        let view = recorder.view();
        let (started, finished, stop) =
            (AtomicU64::new(0), AtomicU64::new(0), AtomicBool::new(false));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut n = 0;
                while !stop.load(Ordering::Relaxed) {
                    n += 1;
                    started.store(n, Ordering::Relaxed);
                    // Whoever sees the sum that includes this value also sees that it was started.
                    fence(Ordering::Release);
                    recorder.record_ns(VALUE);
                    finished.store(n, Ordering::Release);
                }
            });
            // Also when an assertion fails, or the scope would wait for the thread above for ever.
            let _stop = StopOnDrop(&stop);
            // The snapshot, and how many values were being recorded while it was taken, at most.
            let take = || {
                let before = finished.load(Ordering::Acquire);
                let s = view.snapshot();
                fence(Ordering::Acquire);
                (s, started.load(Ordering::Relaxed) - before)
            };
            let (mut previous, mut during_previous) = take();
            let mut outside = 0;
            for _ in 0..snapshots {
                let (s, during) = take();
                let counted = s.count * VALUE;
                assert!(s.sum >= counted, "{s:?}");
                assert!(s.sum - counted <= during * VALUE, "{s:?}, {during}");
                let interval = s.since(&previous);
                let counted = interval.count * VALUE;
                assert!(
                    interval.sum <= counted + during * VALUE
                        && interval.sum + during_previous * VALUE >= counted,
                    "{interval:?}, {during_previous} and {during}"
                );
                outside +=
                    u32::from(interval.mean() > interval.max() || interval.mean() < interval.min());
                (previous, during_previous) = (s, during);
            }
            // Not a requirement: what the documentation warns of.
            println!("{outside} of {snapshots} interval means were outside [min, max]");
        });
        // Nothing is recorded any more: exact.
        let s = view.snapshot();
        assert_eq!(s.sum, s.count * VALUE);
        assert_eq!(ns(s.mean()), Some(VALUE));
    }

    #[cfg(not(miri))]
    mod properties {
        use super::*;
        use proptest::prelude::*;

        /// Values of every magnitude.
        fn value() -> impl Strategy<Value = u64> {
            (0u32..64, any::<u64>()).prop_map(|(shift, v)| v >> shift)
        }

        proptest! {
            #[test]
            fn index_is_monotonic_and_inside_its_bounds(a in value(), b in value()) {
                let (low, high) = bounds(index(a));
                prop_assert!((low..=high).contains(&a));
                prop_assert_eq!(a <= b, index(a) < index(b) || (index(a) == index(b) && a <= b));
            }

            #[test]
            fn snapshot_matches_the_values(
                mut values in prop::collection::vec(value(), 1..400),
                quantiles in prop::collection::vec(0.0f64..=1.0, 1..8),
            ) {
                let s = recorded(&values);
                values.sort_unstable();
                prop_assert_eq!(s.count(), values.len() as u64);
                prop_assert_eq!(ns(s.min()), values.first().copied());
                prop_assert_eq!(ns(s.max()), values.last().copied());
                let sum = values.iter().fold(0u64, |a, &b| a.saturating_add(b));
                prop_assert_eq!(ns(s.sum()), (sum != u64::MAX).then_some(sum));
                let counted: u64 = s.buckets().map(|b| b.count).sum();
                prop_assert_eq!(counted, values.len() as u64);
                for bucket in s.buckets() {
                    let within = |v: &&u64| (bucket.low_ns..=bucket.high_ns).contains(*v);
                    prop_assert_eq!(values.iter().filter(within).count() as u64, bucket.count);
                }
                for q in quantiles.into_iter().chain([0.0, 0.5, 0.9, 0.99, 0.999, 1.0]) {
                    // The definition, by brute force.
                    let r = (1..=values.len())
                        .find(|r| *r as f64 / values.len() as f64 >= q)
                        .unwrap();
                    let truth = values[r - 1];
                    let got = ns(s.value_at_quantile(q)).unwrap();
                    prop_assert!(got >= truth, "{} < {}", got, truth);
                    prop_assert!(got - truth <= truth / 64, "{} > {}", got, truth);
                }
            }

            #[test]
            fn an_interval_matches_recording_it_alone(
                before in prop::collection::vec(value(), 0..200),
                during in prop::collection::vec(value(), 0..200),
            ) {
                let mut recorder = Recorder::new();
                let view = recorder.view();
                before.iter().for_each(|&v| recorder.record_ns(v));
                let earlier = view.snapshot();
                during.iter().for_each(|&v| recorder.record_ns(v));
                let later = view.snapshot();
                let interval = later.since(&earlier);
                let alone = recorded(&during);
                prop_assert_eq!(&interval.buckets, &alone.buckets);
                prop_assert_eq!(interval.count(), alone.count());
                if later.sum().is_some() {
                    prop_assert_eq!(interval.sum(), alone.sum());
                } else if !during.is_empty() {
                    prop_assert_eq!(interval.sum(), None);
                }
                match (ns(alone.min()), ns(alone.max())) {
                    (Some(min), Some(max)) => {
                        let (low, high) = (ns(interval.min()).unwrap(), ns(interval.max()).unwrap());
                        prop_assert!(low <= min && min - low <= low / 64);
                        prop_assert!(high >= max && high - max <= max / 64);
                        if max > before.iter().copied().max().unwrap_or(0) {
                            prop_assert_eq!(high, max);
                        }
                        if min < before.iter().copied().min().unwrap_or(u64::MAX) {
                            prop_assert_eq!(low, min);
                        }
                    }
                    _ => prop_assert_eq!((interval.min(), interval.max()), (None, None)),
                }
            }
        }
    }
}
