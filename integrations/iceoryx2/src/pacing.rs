//! Replay pacing: absolute schedule from the first record, sleep on a monotonic clock, short final spin.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::Error;

/// The slowest speed factor accepted: a one-second gap becomes about 11.6 days.
pub const MIN_SPEED: f64 = 1e-6;
/// The fastest speed factor accepted.
pub const MAX_SPEED: f64 = 1e9;

/// How closely a paced replay kept to the recorded schedule.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PacingReport {
    /// The speed factor (2.0 replays twice as fast as recorded).
    pub speed: f64,
    /// Sends later than this counted as late, in nanoseconds.
    pub late_threshold_ns: u64,
    /// Messages paced.
    pub sends: u64,
    /// Sends that started more than `late_threshold_ns` after their scheduled time.
    pub late_sends: u64,
    /// The latest send, in nanoseconds after its scheduled time.
    pub max_late_ns: u64,
    /// Sequence number of that send.
    pub max_late_seq: Option<u64>,
    /// Its scheduled time, nanoseconds after the replay started.
    pub max_late_scheduled_ns: u64,
    /// Its actual time, nanoseconds after the replay started.
    pub max_late_actual_ns: u64,
    /// Mean lateness over all sends, in nanoseconds.
    pub mean_late_ns: u64,
    /// When the last send was scheduled, nanoseconds after the replay started.
    pub last_scheduled_ns: u64,
    /// When it actually started.
    pub last_actual_ns: u64,
}

pub(crate) struct Pacer {
    speed: f64,
    spin: Duration,
    late_threshold: Duration,
    start: Option<(Instant, u64)>,
    last_offset: Duration,
    total_late_ns: u128,
    pub(crate) report: PacingReport,
}

/// The longest single sleep, so a stop request is noticed within this time.
const MAX_SLEEP: Duration = Duration::from_millis(50);

impl Pacer {
    pub(crate) fn new(speed: f64, spin: Duration, late_threshold: Duration) -> Pacer {
        Pacer {
            speed,
            spin,
            late_threshold,
            start: None,
            last_offset: Duration::ZERO,
            total_late_ns: 0,
            report: PacingReport {
                speed,
                late_threshold_ns: u64::try_from(late_threshold.as_nanos()).unwrap_or(u64::MAX),
                ..PacingReport::default()
            },
        }
    }

    /// The schedule offset of a record: its `local_ts_ns` after the first record's, divided by the speed. Computed
    /// from the first record every time, so rounding never accumulates. Never earlier than the previous record's
    /// (a wall clock that stepped back while recording doesn't reorder the replay). Gaps are kept as recorded,
    /// scaled: a pause between recording runs, or a forward step of the wall clock while recording, is waited out.
    fn offset(&mut self, seq: u64, local_ts_ns: u64, base: u64) -> Result<Duration, Error> {
        let delta = local_ts_ns.saturating_sub(base) as f64 / self.speed;
        let offset = Duration::try_from_secs_f64(delta / 1e9).map_err(|_| {
            Error::Usage(format!(
                "record {seq} would be sent {delta:e} ns after the first one at speed {}, more than a clock can \
                 represent; choose a higher speed",
                self.speed
            ))
        })?;
        let offset = offset.max(self.last_offset);
        self.last_offset = offset;
        Ok(offset)
    }

    /// Waits until the record's scheduled time and returns it, or `None` if `stop` was set while waiting.
    ///
    /// # Errors
    ///
    /// [`Error::Usage`] if the scheduled time can't be represented (a tiny speed and a long gap).
    pub(crate) fn wait(
        &mut self,
        seq: u64,
        local_ts_ns: u64,
        stop: &AtomicBool,
    ) -> Result<Option<Instant>, Error> {
        let (start, base) = *self
            .start
            .get_or_insert_with(|| (Instant::now(), local_ts_ns));
        let offset = self.offset(seq, local_ts_ns, base)?;
        let target = start.checked_add(offset).ok_or_else(|| {
            Error::Usage(format!(
                "record {seq} would be sent {offset:?} after the first one, more than this clock can represent; \
                 choose a higher speed"
            ))
        })?;
        loop {
            let now = Instant::now();
            if now >= target {
                return Ok(Some(target));
            }
            if stop.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let remaining = target - now;
            if remaining > self.spin {
                std::thread::sleep((remaining - self.spin).min(MAX_SLEEP));
            } else {
                std::hint::spin_loop();
            }
        }
    }

    /// Records that the send of `seq`, scheduled at `scheduled`, started at `actual`.
    pub(crate) fn sent(&mut self, seq: u64, scheduled: Instant, actual: Instant) {
        let Some((start, _)) = self.start else { return };
        let late = actual.saturating_duration_since(scheduled);
        let late_ns = u64::try_from(late.as_nanos()).unwrap_or(u64::MAX);
        let r = &mut self.report;
        r.sends += 1;
        self.total_late_ns = self.total_late_ns.saturating_add(u128::from(late_ns));
        r.mean_late_ns = (self.total_late_ns / u128::from(r.sends)) as u64;
        let since =
            |t: Instant| u64::try_from(t.duration_since(start).as_nanos()).unwrap_or(u64::MAX);
        r.last_scheduled_ns = since(scheduled);
        r.last_actual_ns = since(actual);
        if late > self.late_threshold {
            r.late_sends += 1;
        }
        if r.max_late_seq.is_none() || late_ns > r.max_late_ns {
            r.max_late_ns = late_ns;
            r.max_late_seq = Some(seq);
            r.max_late_scheduled_ns = r.last_scheduled_ns;
            r.max_late_actual_ns = r.last_actual_ns;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_follows_recorded_deltas_scaled_by_speed() {
        let mut p = Pacer::new(2.0, Duration::from_micros(200), Duration::from_millis(1));
        assert_eq!(
            p.offset(0, 1_000_000_000, 1_000_000_000).unwrap(),
            Duration::ZERO
        );
        assert_eq!(
            p.offset(1, 1_010_000_000, 1_000_000_000).unwrap(),
            Duration::from_millis(5)
        );
        // A timestamp before the previous one is sent right after it, never earlier.
        assert_eq!(
            p.offset(2, 1_004_000_000, 1_000_000_000).unwrap(),
            Duration::from_millis(5)
        );
    }

    #[test]
    fn long_schedules_do_not_drift() {
        let mut p = Pacer::new(1.0, Duration::ZERO, Duration::ZERO);
        // One message per microsecond for 10 days: the offset is exact, not a sum of rounded steps.
        let ten_days_ns = 10 * 86_400 * 1_000_000_000u64;
        assert_eq!(
            p.offset(0, ten_days_ns, 0).unwrap(),
            Duration::from_nanos(ten_days_ns)
        );
    }

    #[test]
    fn wait_sleeps_until_the_scheduled_time_and_reports_lateness() {
        let stop = AtomicBool::new(false);
        let mut p = Pacer::new(1.0, Duration::from_micros(200), Duration::from_millis(1));
        let t0 = p.wait(0, 0, &stop).unwrap().unwrap();
        p.sent(0, t0, t0);
        let t1 = p.wait(1, 20_000_000, &stop).unwrap().unwrap();
        let now = Instant::now();
        assert!(now >= t1);
        assert!(t1.duration_since(t0) >= Duration::from_millis(20));
        p.sent(1, t1, now);
        assert_eq!(p.report.sends, 2);
        assert_eq!(p.report.last_scheduled_ns, 20_000_000);
    }

    #[test]
    fn wait_returns_none_when_stopped() {
        let stop = AtomicBool::new(false);
        let mut p = Pacer::new(1.0, Duration::from_micros(200), Duration::from_millis(1));
        p.wait(0, 0, &stop).unwrap().unwrap();
        stop.store(true, Ordering::Relaxed);
        assert!(p.wait(1, 60_000_000_000, &stop).unwrap().is_none());
    }

    /// Tiny speeds and long gaps are an error, not a panic (reproduced in review with 1e-300, and with 1e-25 and a
    /// one-second gap).
    #[test]
    fn unrepresentable_schedules_are_errors() {
        let stop = AtomicBool::new(false);
        for (speed, gap) in [(1e-300, 1), (1e-25, 1_000_000_000), (1e-12, u64::MAX)] {
            let mut p = Pacer::new(speed, Duration::ZERO, Duration::ZERO);
            p.wait(0, 0, &stop).unwrap().unwrap();
            let err = p.wait(1, gap, &stop).unwrap_err();
            assert!(err.to_string().contains("choose a higher speed"), "{err}");
        }
    }
}
