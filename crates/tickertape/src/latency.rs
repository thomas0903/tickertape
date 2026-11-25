//! Latency histograms: a [`Recorder`] for durations, and [`RecordAge`] for the age of the records a reader returns
//! (ADR 0047). From version 1.1.
//!
//! Nothing here runs unless the application creates it: the writer and the readers record nothing by themselves. There
//! is no thread, no global state and nothing is written to the journal.
//!
//! **One thread records, any thread reads.** [`Recorder::record`] takes `&mut self`; [`Recorder::view`] gives a
//! [`View`] that any thread of the process can keep and take a [`Snapshot`] from, without a lock and without the
//! recording thread ever waiting. Histograms only grow. For the values of an interval, subtract two snapshots with
//! [`Snapshot::since`].
//!
//! **What the numbers are.** Values are nanoseconds in a `u64`. The smallest and the largest are kept exactly, every
//! value is counted (none is sampled, clamped or out of range), and a quantile is the upper bound of a bucket that is
//! at most 1/64 (1.6 %) of its lower bound wide, so it is never below the true value and at most 1.6 % above it. The
//! bucket layout itself is not promised; [`Snapshot::buckets`] gives the bounds.
//!
//! **What they are not.** A duration around a call is a *service time*: it says how long the call took, not how late
//! the work behind it was. If one call stalls for 5 ms, everything queued behind it is late too, but the histogram has
//! one 5 ms value (coordinated omission). A program that knows when an event should have been handled can record the
//! time since then instead:
//!
//! ```
//! use std::time::{Duration, Instant};
//! use tickertape::latency::Recorder;
//!
//! let mut response = Recorder::new();
//! let view = response.view(); // give this to a monitoring thread
//! let period = Duration::from_micros(100);
//! let mut intended = Instant::now();
//! for _ in 0..3 {
//!     while Instant::now() < intended {} // the schedule
//!     // ... handle the event, for example append it ...
//!     response.record(intended.elapsed()); // from when it was due, not from when the call started
//!     intended += period;
//! }
//! let snapshot = view.snapshot();
//! assert_eq!(snapshot.count(), 3);
//! assert!(snapshot.value_at_quantile(0.99) <= snapshot.max());
//! ```
//!
//! The same works around [`Writer::checkpoint`](crate::Writer::checkpoint) or any other call. A call that is still
//! stuck has no value yet, and what the clock costs is part of the value (subtracting it is not attempted).

mod age;
mod histogram;
#[cfg(all(test, loom))]
mod protocol_model;

pub use age::{RecordAge, RecordAgeSnapshot, RecordAgeView};
pub use histogram::{Bucket, Snapshot};

use histogram::Histogram;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

/// Records durations into a histogram, from one thread; for the application's own measurements.
///
/// Recording is a few loads and stores: it doesn't allocate, lock, or make a system call, and cannot fail. The
/// histogram (about 30 KB) is allocated and written once by [`Recorder::new`], so that recording does not take page
/// faults on it later (in a child after `fork` it does, once per page).
#[derive(Default)]
pub struct Recorder {
    histogram: Arc<Histogram>,
}

impl Recorder {
    /// An empty histogram.
    pub fn new() -> Recorder {
        Recorder::default()
    }

    /// Records one duration. More than `u64::MAX` nanoseconds (about 584 years) counts as `u64::MAX`.
    #[inline]
    pub fn record(&mut self, value: Duration) {
        self.record_ns(u64::try_from(value.as_nanos()).unwrap_or(u64::MAX));
    }

    /// Records one duration in nanoseconds.
    #[inline]
    pub fn record_ns(&mut self, ns: u64) {
        // The exclusive borrow is what `Histogram::record` relies on: no other thread records at the same time.
        self.histogram.record(ns);
    }

    /// A handle to read this histogram from any thread, also after the recorder is dropped.
    pub fn view(&self) -> View {
        View {
            histogram: Arc::clone(&self.histogram),
        }
    }
}

impl fmt::Debug for Recorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Recorder").finish_non_exhaustive()
    }
}

/// A handle to read a [`Recorder`]'s histogram while it is being recorded, from any thread of the process.
///
/// It keeps the histogram alive, so it can be read after the recorder is dropped. In a forked child it is a frozen
/// copy.
#[derive(Clone)]
pub struct View {
    histogram: Arc<Histogram>,
}

impl View {
    /// Copies the histogram (allocates about 30 KB).
    ///
    /// It takes no lock and the recording thread never waits for it. A cache line that a snapshot has read costs the
    /// recording thread one cache miss at its next update there; once a second or slower is the intended use.
    ///
    /// A snapshot taken while values are being recorded is **not a prefix** of them: it can miss the n-th value and
    /// count the one after it. [`Snapshot`] says what holds nevertheless.
    pub fn snapshot(&self) -> Snapshot {
        self.histogram.snapshot()
    }
}

impl fmt::Debug for View {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("View").finish_non_exhaustive()
    }
}
