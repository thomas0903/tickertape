//! The age of records when a reader returns them (ADR 0047).

use super::{Recorder, Snapshot, View};
use crate::record::Record;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Ages of records as a reader returns them: the wall clock ([`clock::realtime_ns`](crate::clock::realtime_ns)) after
/// the read, minus the record's `local_ts_ns`. From version 1.1.
///
/// The code that calls the reader counts each record with [`RecordAge::observe`], right after the call that returned
/// it, and declares when its reader is **live**:
///
/// ```no_run
/// use std::time::Duration;
/// use tickertape::latency::RecordAge;
/// use tickertape::{Reader, ReaderConfig};
///
/// # fn main() -> tickertape::Result<()> {
/// let mut reader = Reader::open("/tmp/journal", ReaderConfig::default())?;
/// let mut age = RecordAge::new();
/// let view = age.view(); // give this to a monitoring thread
/// loop {
///     let record = match reader.try_next()? {
///         Some(record) => Some(record),
///         None => {
///             age.set_live(true); // caught up: from here on every record counts
///             reader.next_timeout(Some(Duration::from_secs(1)))?
///         }
///     };
///     if let Some(record) = record {
///         age.observe(&record); // first, then handle it
///         println!("{}", record.seq);
///     }
/// #   if view.snapshot().age.count() > 10 { break; }
/// }
/// # Ok(())
/// # }
/// ```
///
/// # What the age is, and is not
///
/// It starts where whoever appended the record took `local_ts_ns`, which can be long before the append (the receive
/// time of a market-data packet is the usual choice). It is the writer-to-reader latency only if the timestamp was taken
/// right before [`Writer::append`](crate::Writer::append): the time of the commit is not recorded anywhere. It ends when
/// the record is handed to [`RecordAge::observe`], so it contains the reader's wait and the time the record spent behind
/// others in a backlog. The library can't check any of this, and a timestamp that is not nanoseconds since the Unix
/// epoch (uptime, for example) gives an age that means nothing.
///
/// # Which records count
///
/// In this order; each record is counted exactly once:
///
/// 1. **Not live:** not in the histogram. Counted as [`before_live`](RecordAgeSnapshot::before_live) until the first
///    [`set_live(true)`](RecordAge::set_live), and as
///    [`after_discontinuity`](RecordAgeSnapshot::after_discontinuity) later. The age of a replayed record says when it
///    was recorded, not how fast anything is. A new counter is not live. Declare it live once the reader has caught up
///    or has been placed at the end of the journal, and not live before reading on after a
///    [`Reader::seek`](crate::Reader::seek) or [`Error::Discarded`](crate::Error::Discarded). While live, every record
///    counts, also those of a backlog that builds up later: a slow consumer's lateness is what there is to measure. The
///    declaration is not checked: a forgotten `set_live(false)` before a seek backwards puts a replay into the
///    histogram.
/// 2. **`local_ts_ns` is 0 (not set):** counted as [`unset`](RecordAgeSnapshot::unset), not in the histogram.
/// 3. **`local_ts_ns` is later than the clock:** counted as [`ahead`](RecordAgeSnapshot::ahead), not in the histogram,
///    and never as an age of 0.
/// 4. Everything else goes into the histogram.
///
/// # Clocks
///
/// **When `ahead` is not 0, the minimum and every percentile are biased upwards**, because the missing records are the
/// fastest ones. That happens after the clock stepped back, and with a constant offset: hardware timestamps (NIC, PHC), a
/// clock on TAI (37 s ahead of UTC), or another clock that is a few microseconds fast. A constant offset the other way
/// adds itself to every age, and nothing shows it. Two processes on one machine (also in different containers) that
/// both use `CLOCK_REALTIME` share the clock; if it is stepped, the result moves by the step: forward shows as a false
/// maximum, backward as `ahead`. Use a clock that is slewed. On macOS `realtime_ns` advances in steps of 1 µs, so
/// shorter ages are 0 and a finer timestamp can look ahead.
///
/// One thread records (`&mut self`); [`RecordAge::view`] reads it from any thread.
#[derive(Default)]
pub struct RecordAge {
    age: Recorder,
    counts: Arc<Counts>,
    live: bool,
    was_live: bool,
}

/// The records that are not in the histogram.
#[derive(Default)]
struct Counts {
    before_live: AtomicU64,
    after_discontinuity: AtomicU64,
    unset: AtomicU64,
    ahead: AtomicU64,
}

/// Adds one. A load and a store: only the thread that holds the `RecordAge` exclusively does this.
fn increment(counter: &AtomicU64) {
    let n = counter.load(Ordering::Relaxed).wrapping_add(1);
    counter.store(n, Ordering::Relaxed);
}

impl RecordAge {
    /// An empty counter, not live.
    pub fn new() -> RecordAge {
        RecordAge::default()
    }

    /// Declares whether the reader is live: whether the records it returns from now on are new ones rather than history
    /// (see [`RecordAge`] → Which records count).
    pub fn set_live(&mut self, live: bool) {
        self.live = live;
        self.was_live |= live;
    }

    /// Reads the wall clock and counts the record. The age, if it went into the histogram.
    #[inline]
    pub fn observe(&mut self, record: &Record<'_>) -> Option<Duration> {
        self.observe_at(record.local_ts_ns, crate::clock::realtime_ns())
    }

    /// As [`RecordAge::observe`], from the two times: for an [`OwnedRecord`](crate::OwnedRecord), a clock read already
    /// taken, or a test. Both are nanoseconds since the Unix epoch.
    pub fn observe_at(&mut self, local_ts_ns: u64, now_ns: u64) -> Option<Duration> {
        if !self.live {
            increment(if self.was_live {
                &self.counts.after_discontinuity
            } else {
                &self.counts.before_live
            });
            return None;
        }
        if local_ts_ns == 0 {
            increment(&self.counts.unset);
            return None;
        }
        let Some(age) = now_ns.checked_sub(local_ts_ns) else {
            increment(&self.counts.ahead);
            return None;
        };
        self.age.record_ns(age);
        Some(Duration::from_nanos(age))
    }

    /// A handle to read this counter from any thread, also after it is dropped.
    pub fn view(&self) -> RecordAgeView {
        RecordAgeView {
            age: self.age.view(),
            counts: Arc::clone(&self.counts),
        }
    }
}

impl fmt::Debug for RecordAge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordAge")
            .field("live", &self.live)
            .finish_non_exhaustive()
    }
}

/// A handle to read a [`RecordAge`] while records are being counted, from any thread of the process.
#[derive(Clone)]
pub struct RecordAgeView {
    age: View,
    counts: Arc<Counts>,
}

impl RecordAgeView {
    /// Copies the histogram and the counts, without a lock (as [`View::snapshot`], which see for the cost). Taken while
    /// records are being counted, the parts are not from one moment: each is at least what it was when this call
    /// started.
    pub fn snapshot(&self) -> RecordAgeSnapshot {
        RecordAgeSnapshot {
            age: self.age.snapshot(),
            before_live: self.counts.before_live.load(Ordering::Relaxed),
            after_discontinuity: self.counts.after_discontinuity.load(Ordering::Relaxed),
            unset: self.counts.unset.load(Ordering::Relaxed),
            ahead: self.counts.ahead.load(Ordering::Relaxed),
        }
    }
}

impl fmt::Debug for RecordAgeView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordAgeView").finish_non_exhaustive()
    }
}

/// What a [`RecordAge`] counted. Everything is cumulative since it was created.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RecordAgeSnapshot {
    /// The ages. Biased upwards when `ahead` is not 0 (see [`RecordAge`] → Clocks).
    pub age: Snapshot,
    /// Records counted before the first declaration that the reader is live; not in `age`.
    pub before_live: u64,
    /// Records counted later while the reader was declared not live; not in `age`.
    pub after_discontinuity: u64,
    /// Live records without a `local_ts_ns`; not in `age`.
    pub unset: u64,
    /// Live records whose `local_ts_ns` was later than the observer's clock; not in `age`.
    pub ahead: u64,
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    fn counts(age: &RecordAge) -> (u64, u64, u64, u64, u64) {
        let s = age.view().snapshot();
        (
            s.age.count(),
            s.before_live,
            s.after_discontinuity,
            s.unset,
            s.ahead,
        )
    }

    #[test]
    fn every_record_is_counted_once_by_the_first_rule_that_applies() {
        let mut age = RecordAge::new();
        // Before the first declaration: whatever the timestamps are.
        assert_eq!(age.observe_at(100, 150), None);
        assert_eq!(age.observe_at(0, 150), None);
        assert_eq!(age.observe_at(200, 150), None);
        // Declaring it not live again changes nothing.
        age.set_live(false);
        assert_eq!(age.observe_at(100, 150), None);
        assert_eq!(counts(&age), (0, 4, 0, 0, 0));

        age.set_live(true);
        assert_eq!(age.observe_at(100, 150), Some(Duration::from_nanos(50)));
        assert_eq!(age.observe_at(0, 150), None);
        assert_eq!(age.observe_at(0, 0), None); // unset before anything about the clock
        assert_eq!(age.observe_at(151, 150), None);
        assert_eq!(age.observe_at(u64::MAX, 0), None); // the clock before the epoch
        assert_eq!(age.observe_at(150, 150), Some(Duration::ZERO)); // a real age of 0
        assert_eq!(counts(&age), (2, 4, 0, 2, 2));

        // A backlog that builds up while live counts, however old.
        assert_eq!(
            age.observe_at(1, u64::MAX),
            Some(Duration::from_nanos(u64::MAX - 1))
        );
        assert_eq!(counts(&age), (3, 4, 0, 2, 2));

        age.set_live(false);
        assert_eq!(age.observe_at(100, 150), None);
        assert_eq!(age.observe_at(0, 150), None);
        assert_eq!(counts(&age), (3, 4, 2, 2, 2));
        age.set_live(true);
        assert_eq!(age.observe_at(100, 103), Some(Duration::from_nanos(3)));
        let view = age.view();
        drop(age);
        let s = view.snapshot();
        assert_eq!(s.age.min(), Some(Duration::ZERO));
        assert_eq!(s.age.max(), Some(Duration::from_nanos(u64::MAX - 1)));
        assert_eq!((s.age.count(), s.after_discontinuity), (4, 2));
    }

    #[test]
    fn meaningless_ages_do_not_overflow() {
        let mut age = RecordAge::default();
        age.set_live(true);
        for _ in 0..11 {
            age.observe_at(1_000, 1_800_000_000_000_001_000);
        }
        let s = age.view().snapshot().age;
        assert_eq!((s.count(), s.sum(), s.mean()), (11, None, None));
    }

    #[test]
    #[cfg_attr(miri, ignore = "reads the wall clock")]
    fn observe_reads_the_wall_clock() {
        let mut age = RecordAge::new();
        age.set_live(true);
        let record = |local_ts_ns| Record {
            seq: 0,
            schema_id: 1,
            venue_ts_ns: 5,
            local_ts_ns,
            payload: b"",
        };
        let sent = crate::clock::realtime_ns() - 1_000_000;
        let got = age.observe(&record(sent)).unwrap();
        assert!((Duration::from_millis(1)..Duration::from_secs(600)).contains(&got));
        assert_eq!(age.observe(&record(u64::MAX)), None);
        assert_eq!(age.observe(&record(0)), None);
        assert_eq!(counts(&age), (1, 0, 0, 1, 1));
        assert!(format!("{age:?} {:?}", age.view()).len() < 100);
    }

    #[test]
    fn types_cross_threads_as_documented() {
        fn send<T: Send>() {}
        fn sync<T: Sync>() {}
        send::<RecordAge>();
        send::<RecordAgeView>();
        sync::<RecordAgeView>();
    }
}
