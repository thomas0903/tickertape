//! The histograms of a writer (ADR 0047): what [`WriterConfig::latency`](crate::WriterConfig::latency) turns on.

use super::histogram::Histogram;
use super::{Recorder, View, nanos};
use crate::config::LatencyMode;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A writer's latency histograms, from [`Writer::latency`](crate::Writer::latency). From version 1.1.
///
/// Each accessor returns `None` for what the writer's [`LatencyMode`] does not measure. The [`View`]s can be cloned (or
/// this whole value), kept by another thread and read after the writer is dropped; they are readable in the writer's
/// process only.
///
/// Every value is wall time as the thread saw it, by the monotonic clock ([`Instant`]), so it includes being descheduled.
/// Nothing is sampled and nothing is subtracted for the clock. Calls that fail are not recorded: the caller has the
/// error. `docs/operations.md` → "How long the writer's calls take" has the measured cost of recording, names for the
/// metrics, and how the numbers relate to the published benchmark tables (they are not the same).
#[derive(Debug, Clone)]
pub struct WriterLatency {
    append: Option<View>,
    batch: Option<View>,
    roll: Option<View>,
    time_to_durable: Option<View>,
}

impl WriterLatency {
    /// How long each successful call of [`Writer::append`](crate::Writer::append),
    /// [`append_with`](crate::Writer::append_with) or [`try_append_with`](crate::Writer::try_append_with) took. Only with
    /// [`LatencyMode::All`].
    ///
    /// **This is a service time, not a response time or an end-to-end latency.** If one append stalls for 5 ms, the
    /// events queued behind it are late too, but the histogram has one 5 ms value (coordinated omission): the writer
    /// cannot know when an event should have been appended. A program that does know can record the time since then in a
    /// [`Recorder`] of its own, as the [module documentation](super) shows.
    ///
    /// **A call or a checkpoint that is still stuck has no value yet**, here and in the other histograms.
    /// `tickertape status` shows that: a head that stands still, a growing exposure age.
    ///
    /// It runs from the entry of the call, before any check, to just before it returns the sequence number. It includes
    /// the checks, the payload copy or the caller's `fill` closure, the CRC, the commit, the wake-up of blocked readers,
    /// the hand-off to the writer's helper threads, page faults and the roll, if there is one ([`WriterLatency::roll`]).
    /// It does not include building the record before the call, or recording the value itself. A `fill` that declines or
    /// panics is not recorded, although its clean-up is slow.
    ///
    /// Parts of both clock reads fall inside the interval, and the clock ticks: with a coarse clock (41.7 ns per tick on
    /// Apple silicon) a short append often shows as 0 or one tick, and longer ones as multiples of the tick; with a fine
    /// one even an empty interval measures some nanoseconds. Nothing is subtracted for either. Reading the
    /// clock also changes what it measures: it limits how far consecutive appends overlap in the CPU, and it does not
    /// wait for the store buffer to drain, so part of an append's cost can fall outside its interval.
    pub fn append(&self) -> Option<&View> {
        self.append.as_ref()
    }

    /// How long each successful call of [`Writer::append_batch`](crate::Writer::append_batch) that appended at least one
    /// record took: one value per call, whatever the size of the batch. Only with [`LatencyMode::All`].
    ///
    /// It includes the caller's iterator, all its records, rolls and the one wake-up. A batch that stops with an error is
    /// not recorded. What [`WriterLatency::append`] says about service times holds here too.
    pub fn batch(&self) -> Option<&View> {
        self.batch.as_ref()
    }

    /// The time the appending thread spent on each successful roll to a new segment, inside `append_batch` too. With
    /// [`LatencyMode::RollsAndCheckpoints`] and [`LatencyMode::All`].
    ///
    /// It runs from where the append path has found that the record does not fit to where the roll has succeeded, before
    /// the record is written. The [`append`](WriterLatency::append) or [`batch`](WriterLatency::batch) value of the same
    /// call includes it.
    pub fn roll(&self) -> Option<&View> {
        self.roll.as_ref()
    }

    /// The time from a checkpoint request to its durable marker, one value per checkpoint that wrote its marker. With
    /// [`LatencyMode::RollsAndCheckpoints`] and [`LatencyMode::All`].
    ///
    /// It runs from the entry of [`Writer::checkpoint`](crate::Writer::checkpoint), or of the earliest
    /// [`Writer::request_checkpoint`](crate::Writer::request_checkpoint) that the checkpoint covers, until the marker is
    /// durable (and its best-effort extension is done and waiting readers are woken). It includes waiting behind a
    /// checkpoint that is already running and excludes the retention run that follows that checkpoint (a blocking
    /// checkpoint requested while a background one's retention runs still waits for the removal in progress, and that
    /// wait is in its value). Requests that join a waiting
    /// checkpoint give one value, timed from the earliest of them, so the number is never too small.
    ///
    /// It is not the time from a record's append to its durability, which adds the wait for the next request; the age of
    /// records in a durable-only reader shows that ([`RecordAge`](super::RecordAge)). A caller that polls
    /// [`Writer::checkpoint_progress`](crate::Writer::checkpoint_progress) sees a checkpoint later than this says; once
    /// it does, the value is there.
    pub fn time_to_durable(&self) -> Option<&View> {
        self.time_to_durable.as_ref()
    }
}

/// The recording side of everything but `append`: behind a `Box` in the writer, so that it costs the append path nothing.
#[derive(Debug)]
pub(crate) struct WriterRecorders {
    batch: Option<Recorder>,
    roll: Recorder,
    durable: Durable,
    views: WriterLatency,
}

impl WriterRecorders {
    /// The recorder of `append`, and the others, as `mode` asks for them.
    pub(crate) fn new(mode: LatencyMode) -> (Option<Recorder>, Option<Box<WriterRecorders>>) {
        let calls = match mode {
            LatencyMode::Off => return (None, None),
            LatencyMode::RollsAndCheckpoints => false,
            LatencyMode::All => true,
        };
        let append = calls.then(Recorder::new);
        let batch = calls.then(Recorder::new);
        let roll = Recorder::new();
        let durable = Durable::default();
        let views = WriterLatency {
            append: append.as_ref().map(Recorder::view),
            batch: batch.as_ref().map(Recorder::view),
            roll: Some(roll.view()),
            time_to_durable: Some(View {
                histogram: Arc::clone(&durable.histogram),
            }),
        };
        let others = WriterRecorders {
            batch,
            roll,
            durable,
            views,
        };
        (append, Some(Box::new(others)))
    }

    pub(crate) fn batch(&mut self, since: Instant) {
        if let Some(batch) = &mut self.batch {
            batch.record(since.elapsed());
        }
    }

    pub(crate) fn roll(&mut self, since: Instant) {
        self.roll.record(since.elapsed());
    }

    /// For what the writer shares with its checkpoint thread.
    pub(crate) fn durable(&self) -> Durable {
        self.durable.clone()
    }

    pub(crate) fn views(&self) -> &WriterLatency {
        &self.views
    }
}

/// The recorder of `time_to_durable`, shared by the writer's thread and its checkpoint thread.
#[derive(Clone, Default)]
pub(crate) struct Durable {
    histogram: Arc<Histogram>,
}

impl Durable {
    /// Any thread, also at the same time as another.
    pub(crate) fn record(&self, value: Duration) {
        self.histogram.record_shared(nanos(value));
    }
}

impl std::fmt::Debug for Durable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Durable").finish_non_exhaustive()
    }
}
