//! Replay pacing: absolute schedule from the first record, sleep on a monotonic clock, short final spin.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

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
                late_threshold_ns: late_threshold.as_nanos() as u64,
                ..PacingReport::default()
            },
        }
    }

    /// The schedule offset of a record: its `local_ts_ns` after the first record's, divided by the speed. Computed
    /// from the first record every time, so rounding never accumulates. Never earlier than the previous record's
    /// (a wall clock that stepped back while recording doesn't reorder the replay).
    fn offset(&mut self, local_ts_ns: u64, base: u64) -> Duration {
        let delta = local_ts_ns.saturating_sub(base) as f64 / self.speed;
        let offset = Duration::from_secs_f64(delta / 1e9).max(self.last_offset);
        self.last_offset = offset;
        offset
    }

    /// Waits until the record's scheduled time and returns it, or `None` if `stop` was set while waiting.
    pub(crate) fn wait(&mut self, local_ts_ns: u64, stop: &AtomicBool) -> Option<Instant> {
        let (start, base) = *self
            .start
            .get_or_insert_with(|| (Instant::now(), local_ts_ns));
        let target = start + self.offset(local_ts_ns, base);
        loop {
            let now = Instant::now();
            if now >= target {
                return Some(target);
            }
            if stop.load(Ordering::Relaxed) {
                return None;
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
        let late_ns = late.as_nanos() as u64;
        let r = &mut self.report;
        r.sends += 1;
        self.total_late_ns += u128::from(late_ns);
        r.mean_late_ns = (self.total_late_ns / u128::from(r.sends)) as u64;
        r.last_scheduled_ns = scheduled.duration_since(start).as_nanos() as u64;
        r.last_actual_ns = actual.duration_since(start).as_nanos() as u64;
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
        assert_eq!(p.offset(1_000_000_000, 1_000_000_000), Duration::ZERO);
        assert_eq!(
            p.offset(1_010_000_000, 1_000_000_000),
            Duration::from_millis(5)
        );
        // A timestamp before the previous one is sent right after it, never earlier.
        assert_eq!(
            p.offset(1_004_000_000, 1_000_000_000),
            Duration::from_millis(5)
        );
    }

    #[test]
    fn long_schedules_do_not_drift() {
        let mut p = Pacer::new(1.0, Duration::ZERO, Duration::ZERO);
        // One message per microsecond for 10 days: the offset is exact, not a sum of rounded steps.
        let ten_days_ns = 10 * 86_400 * 1_000_000_000u64;
        assert_eq!(p.offset(ten_days_ns, 0), Duration::from_nanos(ten_days_ns));
    }

    #[test]
    fn wait_sleeps_until_the_scheduled_time_and_reports_lateness() {
        let stop = AtomicBool::new(false);
        let mut p = Pacer::new(1.0, Duration::from_micros(200), Duration::from_millis(1));
        let t0 = p.wait(0, &stop).unwrap();
        p.sent(0, t0, t0);
        let t1 = p.wait(20_000_000, &stop).unwrap();
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
        p.wait(0, &stop).unwrap();
        stop.store(true, Ordering::Relaxed);
        assert!(p.wait(60_000_000_000, &stop).is_none());
    }
}
