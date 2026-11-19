//! Building blocks for `tt-sustained` (ADR 0026): send schedules, reader specifications, lag and per-second
//! accounting, the writer-position file, process CPU usage, CPU pinning and host facts.
//!
//! Everything here is safe Rust. System calls go through `nix` (`getrusage`, `sched_setaffinity`, `statvfs`) and
//! `std::os::unix::fs::FileExt` (`pread`/`pwrite`).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use serde_json::{Value, json};
use tickertape::{Backoff, ReaderConfig, format};

use crate::Result;

const NS_PER_S: u64 = 1_000_000_000;

/// Parses a duration: an integer with a unit suffix `ns`, `us`, `ms`, `s`, `m` or `h`, or a bare integer in seconds.
pub fn parse_duration(text: &str) -> Result<Duration> {
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, unit) = text.split_at(split);
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("invalid duration: {text:?}"))?;
    let multiplier_ns: u64 = match unit {
        "ns" => 1,
        "us" => 1_000,
        "ms" => 1_000_000,
        "" | "s" => NS_PER_S,
        "m" => 60 * NS_PER_S,
        "h" => 3_600 * NS_PER_S,
        _ => {
            return Err(
                format!("invalid duration unit in {text:?} (use ns, us, ms, s, m or h)").into(),
            );
        }
    };
    let ns = value
        .checked_mul(multiplier_ns)
        .ok_or_else(|| format!("duration too long: {text:?}"))?;
    Ok(Duration::from_nanos(ns))
}

/// Formats a duration in the largest unit that represents it exactly (the inverse of [`parse_duration`]).
pub fn format_duration(duration: Duration) -> String {
    let ns = duration.as_nanos();
    for (unit, size) in [
        ("h", 3_600_000_000_000u128),
        ("m", 60_000_000_000),
        ("s", 1_000_000_000),
        ("ms", 1_000_000),
        ("us", 1_000),
    ] {
        if ns >= size && ns % size == 0 {
            return format!("{}{unit}", ns / size);
        }
    }
    if ns == 0 {
        "0s".into()
    } else {
        format!("{ns}ns")
    }
}

fn duration_ns(duration: Duration) -> Result<u64> {
    Ok(u64::try_from(duration.as_nanos())?)
}

/// When the writer sends record `i`, relative to the shared start time `t0`.
///
/// Paced schedules give every record an intended send time. The writer busy-waits until that time, sends every
/// record even when it is late (no skipping, no rebasing) and stores the intended time in `local_ts_ns`, so
/// readers measure from it and a backlog contributes to latency (coordinated omission handled as in `tt-latency`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    /// Record `i` at `floor(i * 1e9 / rate)` ns.
    Steady {
        /// Records per second.
        rate: u64,
    },
    /// On/off bursts: each period starts with a burst of `burst_records` sent at `peak_rate`, then nothing until
    /// the next period. Record `i` is number `j = i % burst_records` of burst `k = i / burst_records`, at
    /// `k * period_ns + floor(j * 1e9 / peak_rate)` ns.
    Bursty {
        /// Records per second during a burst.
        peak_rate: u64,
        /// Length of one on/off period in ns.
        period_ns: u64,
        /// Length of the "on" part of a period in ns (`period_ns * duty / 100`).
        on_ns: u64,
        /// Records per burst: `floor(peak_rate * on_ns / 1e9)`, at least 1.
        burst_records: u64,
    },
    /// As fast as possible: no intended times; the run ends after its duration.
    Unpaced,
}

impl Schedule {
    /// A steady schedule; the rate must be positive.
    pub fn steady(rate: u64) -> Result<Self> {
        if rate == 0 {
            return Err("rate must be positive".into());
        }
        Ok(Schedule::Steady { rate })
    }

    /// A bursty schedule: bursts at `peak_rate` records/s for `duty_pct` percent (1 to 100) of every `period`.
    pub fn bursty(peak_rate: u64, period: Duration, duty_pct: u64) -> Result<Self> {
        if peak_rate == 0 || !(1..=100).contains(&duty_pct) {
            return Err("burst rate must be positive and duty a percentage from 1 to 100".into());
        }
        let period_ns = duration_ns(period)?;
        let on_ns = u64::try_from(u128::from(period_ns) * u128::from(duty_pct) / 100)?;
        let burst_records =
            u64::try_from(u128::from(peak_rate) * u128::from(on_ns) / 1_000_000_000)?;
        if burst_records == 0 {
            return Err(
                "a burst must contain at least one record: raise the rate, period or duty".into(),
            );
        }
        Ok(Schedule::Bursty {
            peak_rate,
            period_ns,
            on_ns,
            burst_records,
        })
    }

    /// Offset of record `index` from the start time, or `None` for [`Schedule::Unpaced`].
    pub fn offset_ns(&self, index: u64) -> Result<Option<u64>> {
        let offset = match *self {
            Schedule::Steady { rate } => u128::from(index) * 1_000_000_000 / u128::from(rate),
            Schedule::Bursty {
                peak_rate,
                period_ns,
                burst_records,
                ..
            } => {
                let (burst, within) = (index / burst_records, index % burst_records);
                u128::from(burst) * u128::from(period_ns)
                    + u128::from(within) * 1_000_000_000 / u128::from(peak_rate)
            }
            Schedule::Unpaced => return Ok(None),
        };
        Ok(Some(u64::try_from(offset)?))
    }

    /// Intended send time of record `index` for start time `t0`; an error for [`Schedule::Unpaced`] or overflow.
    pub fn intended_ns(&self, t0: u64, index: u64) -> Result<u64> {
        let offset = self
            .offset_ns(index)?
            .ok_or("an unpaced schedule has no intended times")?;
        t0.checked_add(offset)
            .ok_or_else(|| "schedule timestamp overflow".into())
    }

    /// Number of records whose offset is strictly less than `window_ns`, or `None` for [`Schedule::Unpaced`].
    pub fn count_within(&self, window_ns: u64) -> Option<u64> {
        // floor(x) < w for an integer w exactly when x < w, so the count is ceil(w * rate / 1e9).
        let within = |ns: u64, rate: u64| -> u128 {
            (u128::from(ns) * u128::from(rate)).div_ceil(1_000_000_000)
        };
        let count = match *self {
            Schedule::Steady { rate } => within(window_ns, rate),
            Schedule::Bursty {
                peak_rate,
                period_ns,
                burst_records,
                ..
            } => {
                let full = u128::from(window_ns / period_ns) * u128::from(burst_records);
                full + within(window_ns % period_ns, peak_rate).min(u128::from(burst_records))
            }
            Schedule::Unpaced => return None,
        };
        u64::try_from(count).ok()
    }

    /// Mean intended rate in records/s, or `None` for [`Schedule::Unpaced`].
    pub fn mean_rate(&self) -> Option<f64> {
        match *self {
            Schedule::Steady { rate } => Some(rate as f64),
            Schedule::Bursty {
                period_ns,
                burst_records,
                ..
            } => Some(burst_records as f64 * 1e9 / period_ns as f64),
            Schedule::Unpaced => None,
        }
    }

    /// The schedule as JSON with explicit units.
    pub fn to_json(&self) -> Value {
        match *self {
            Schedule::Steady { rate } => {
                json!({"kind": "steady", "rate_records_per_s": rate, "mean_records_per_s": rate})
            }
            Schedule::Bursty {
                peak_rate,
                period_ns,
                on_ns,
                burst_records,
            } => json!({"kind": "bursty", "peak_records_per_s": peak_rate,
                "period_ms": period_ns as f64 / 1e6, "on_ms": on_ns as f64 / 1e6,
                "duty_pct": on_ns as f64 * 100.0 / period_ns as f64, "burst_records": burst_records,
                "mean_records_per_s": self.mean_rate()}),
            Schedule::Unpaced => json!({"kind": "unpaced", "mean_records_per_s": null}),
        }
    }
}

/// How a reader waits for the next record (as `tt-latency --wait`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitMode {
    /// `Backoff::busy_spin()` with wake-ups off: one core at 100 %.
    Spin,
    /// `ReaderConfig::default()`: spins, yields, then blocks on the wake-up region.
    Default,
    /// No spins or yields: block on the wake-up region at once.
    Block,
    /// `Backoff::default()` with wake-ups off: spins, yields, then 10 µs to 1 ms sleeps.
    Sleep,
}

impl WaitMode {
    /// Parses `spin`, `default`, `block` or `sleep`.
    pub fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "spin" => WaitMode::Spin,
            "default" => WaitMode::Default,
            "block" => WaitMode::Block,
            "sleep" => WaitMode::Sleep,
            _ => {
                return Err(
                    format!("wait must be spin, default, block or sleep, not {text:?}").into(),
                );
            }
        })
    }

    /// The name accepted by [`WaitMode::parse`].
    pub fn name(self) -> &'static str {
        match self {
            WaitMode::Spin => "spin",
            WaitMode::Default => "default",
            WaitMode::Block => "block",
            WaitMode::Sleep => "sleep",
        }
    }
}

/// A periodic reader stall: pause for `pause` once every `every`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stall {
    /// How long each stall lasts.
    pub pause: Duration,
    /// Time between stall starts.
    pub every: Duration,
}

/// One reader process of a sustained run, parsed from `kind[:option]...`.
///
/// `kind` is `safe` (`Reader::open`, `pread`) or `mapped` (`Reader::open_mapped`). Options: `durable`
/// (`ReaderConfig::durable_only`), `wait=spin|default|block|sleep`, `delay=<duration>` (after every record) and
/// `stall=<pause>/<every>` (a periodic pause). A reader with a delay or stall is a *lagging* reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderSpec {
    /// Zero-copy mapped reader instead of the safe `pread` reader.
    pub mapped: bool,
    /// Return only records covered by a checkpoint.
    pub durable_only: bool,
    /// How the reader waits for new records.
    pub wait: WaitMode,
    /// Artificial delay after every record.
    pub delay: Option<Duration>,
    /// Artificial periodic stall.
    pub stall: Option<Stall>,
}

impl ReaderSpec {
    /// Parses one reader specification; `default_wait` applies unless the spec sets `wait=`.
    pub fn parse(text: &str, default_wait: WaitMode) -> Result<Self> {
        let mut parts = text.split(':');
        let mapped = match parts.next() {
            Some("mapped") => true,
            Some("safe") => false,
            _ => return Err(format!("reader {text:?} must start with safe or mapped").into()),
        };
        let mut spec = ReaderSpec {
            mapped,
            durable_only: false,
            wait: default_wait,
            delay: None,
            stall: None,
        };
        let mut seen = Vec::new();
        for part in parts {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            if seen.contains(&key) {
                return Err(format!("reader {text:?} repeats {key}").into());
            }
            seen.push(key);
            match (key, value) {
                ("durable", "") => spec.durable_only = true,
                ("wait", value) => spec.wait = WaitMode::parse(value)?,
                ("delay", value) => {
                    let delay = parse_duration(value)?;
                    if delay.is_zero() {
                        return Err("reader delay must be positive".into());
                    }
                    spec.delay = Some(delay);
                }
                ("stall", value) => {
                    let (pause, every) = value
                        .split_once('/')
                        .ok_or("reader stall must be <pause>/<every>, e.g. stall=200ms/5s")?;
                    let stall = Stall {
                        pause: parse_duration(pause)?,
                        every: parse_duration(every)?,
                    };
                    if stall.pause.is_zero() || stall.every <= stall.pause {
                        return Err("reader stall needs 0 < pause < every".into());
                    }
                    spec.stall = Some(stall);
                }
                _ => return Err(format!("unknown reader option {part:?} in {text:?}").into()),
            }
        }
        Ok(spec)
    }

    /// Parses a comma-separated list; `none` means no readers.
    pub fn parse_list(text: &str, default_wait: WaitMode) -> Result<Vec<Self>> {
        if text == "none" {
            return Ok(Vec::new());
        }
        text.split(',')
            .map(|spec| ReaderSpec::parse(spec, default_wait))
            .collect()
    }

    /// Whether the reader is slowed down on purpose.
    pub fn lagging(&self) -> bool {
        self.delay.is_some() || self.stall.is_some()
    }

    /// Canonical text form, accepted by [`ReaderSpec::parse`].
    pub fn label(&self) -> String {
        let mut label = String::from(if self.mapped { "mapped" } else { "safe" });
        if self.durable_only {
            label.push_str(":durable");
        }
        label.push_str(":wait=");
        label.push_str(self.wait.name());
        if let Some(delay) = self.delay {
            label.push_str(&format!(":delay={}", format_duration(delay)));
        }
        if let Some(stall) = self.stall {
            label.push_str(&format!(
                ":stall={}/{}",
                format_duration(stall.pause),
                format_duration(stall.every)
            ));
        }
        label
    }

    /// The reader configuration for this spec (the same mapping as `tt-latency --wait`).
    pub fn reader_config(&self) -> ReaderConfig {
        let config = match self.wait {
            WaitMode::Spin => ReaderConfig::default()
                .backoff(Backoff::busy_spin())
                .notify(false),
            WaitMode::Block => {
                let mut backoff = Backoff::default();
                backoff.spins = 0;
                backoff.yields = 0;
                ReaderConfig::default().backoff(backoff)
            }
            WaitMode::Sleep => ReaderConfig::default().notify(false),
            WaitMode::Default => ReaderConfig::default(),
        };
        config.durable_only(self.durable_only)
    }

    /// The spec as JSON with explicit units.
    pub fn to_json(&self) -> Value {
        json!({"spec": self.label(), "kind": if self.mapped { "mapped" } else { "safe" },
            "durable_only": self.durable_only, "wait": self.wait.name(),
            "latency_kind": if self.durable_only { "durable_only" } else { "visible" },
            "delay_us": self.delay.map(|d| d.as_secs_f64() * 1e6),
            "stall_pause_ms": self.stall.map(|s| s.pause.as_secs_f64() * 1e3),
            "stall_every_ms": self.stall.map(|s| s.every.as_secs_f64() * 1e3),
            "lagging": self.lagging()})
    }
}

/// Journal bytes one record occupies: its frame (header plus payload) rounded up to 8 bytes.
pub fn record_bytes(payload: usize) -> Result<u64> {
    let frame = format::frame_len(payload).ok_or("payload too large")?;
    Ok(format::align8(u64::from(frame)))
}

/// Records a reader is behind the writer: the writer's next sequence number minus the reader's, never negative.
///
/// The two positions are sampled at slightly different times; a reader that has just caught up can appear ahead
/// of a stale writer position, which counts as zero lag.
pub fn lag_records(writer_next: u64, reader_next: u64) -> u64 {
    writer_next.saturating_sub(reader_next)
}

/// Histogram statistics with a unit suffix: `count`, `min_<unit>`, `p50_<unit>`, `p90_<unit>`, `p99_<unit>`,
/// `p99_9_<unit>`, `p99_99_<unit>`, `max_<unit>`. Values are divided by `divisor`; an empty histogram has null
/// statistics.
pub fn quantiles_json(hist: &Histogram<u64>, unit: &str, divisor: f64) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("count".into(), json!(hist.len()));
    let stats = [
        ("min", hist.min()),
        ("p50", hist.value_at_quantile(0.5)),
        ("p90", hist.value_at_quantile(0.9)),
        ("p99", hist.value_at_quantile(0.99)),
        ("p99_9", hist.value_at_quantile(0.999)),
        ("p99_99", hist.value_at_quantile(0.9999)),
        ("max", hist.max()),
    ];
    for (name, value) in stats {
        let value = if hist.is_empty() {
            Value::Null
        } else {
            json!(value as f64 / divisor)
        };
        map.insert(format!("{name}_{unit}"), value);
    }
    Value::Object(map)
}

/// Reader lag statistics in records and in journal bytes (`records * record_bytes`), from a histogram of lag
/// samples in records. `samples` counts the samples.
pub fn lag_json(hist: &Histogram<u64>, record_bytes: u64) -> Value {
    let records = quantiles_json(hist, "records", 1.0);
    let bytes = quantiles_json(hist, "bytes", 1.0 / record_bytes as f64);
    let mut map = serde_json::Map::new();
    map.insert("samples".into(), json!(hist.len()));
    for source in [records, bytes] {
        if let Value::Object(fields) = source {
            map.extend(fields.into_iter().filter(|(k, _)| k != "count"));
        }
    }
    Value::Object(map)
}

/// Statistics of one wall-clock second in one process, keyed by the start time of each operation in that process
/// (the writer: when a send or checkpoint started; a reader: when it observed a record).
#[derive(Debug)]
pub struct SecondBucket {
    /// Unix second.
    pub second: u64,
    /// Records sent (writer) or received (reader).
    pub records: u64,
    /// Sends that started more than 1 µs after their intended time.
    pub late: u64,
    /// Largest send-time minus intended-time, ns.
    pub schedule_delay_max_ns: u64,
    /// Append (writer) or record latency (reader), ns.
    pub latency: Histogram<u64>,
    /// Checkpoints started in this second.
    pub checkpoints: u64,
    /// Longest of those checkpoints, ns.
    pub checkpoint_max_ns: u64,
    /// Appends that rolled to a new segment.
    pub rolls: u64,
    /// Longest of those appends, ns.
    pub roll_max_ns: u64,
    /// Records skipped because retention removed them before this reader got there.
    pub skipped: u64,
}

impl SecondBucket {
    fn new(second: u64) -> Result<Self> {
        Ok(SecondBucket {
            second,
            records: 0,
            late: 0,
            schedule_delay_max_ns: 0,
            latency: Histogram::new(3)?,
            checkpoints: 0,
            checkpoint_max_ns: 0,
            rolls: 0,
            roll_max_ns: 0,
            skipped: 0,
        })
    }

    fn reset(&mut self, second: u64) {
        self.second = second;
        self.records = 0;
        self.late = 0;
        self.schedule_delay_max_ns = 0;
        self.latency.reset();
        self.checkpoints = 0;
        self.checkpoint_max_ns = 0;
        self.rolls = 0;
        self.roll_max_ns = 0;
        self.skipped = 0;
    }

    /// The bucket as one `ops` line of a per-process series file.
    pub fn to_json(&self) -> Value {
        let us = |ns: u64| ns as f64 / 1000.0;
        let latency_max = (!self.latency.is_empty()).then(|| us(self.latency.max()));
        let latency_p99 =
            (!self.latency.is_empty()).then(|| us(self.latency.value_at_quantile(0.99)));
        json!({"kind": "ops", "second": self.second, "records": self.records, "late_records": self.late,
            "schedule_delay_max_us": us(self.schedule_delay_max_ns), "latency_max_us": latency_max,
            "latency_p99_us": latency_p99, "checkpoints": self.checkpoints,
            "checkpoint_max_us": us(self.checkpoint_max_ns), "rolls": self.rolls, "roll_max_us": us(self.roll_max_ns),
            "skipped_records": self.skipped})
    }
}

/// Per-second aggregation on a measuring thread. `bucket(t_ns)` returns the bucket of the second containing
/// `t_ns`; moving to a new second hands the finished bucket's JSON to `emit` (normally a channel to the sampler
/// thread, which writes it). The histogram is reused, so steady-state aggregation does not allocate.
#[derive(Debug)]
pub struct Seconds {
    current: SecondBucket,
    started: bool,
}

impl Seconds {
    /// An empty aggregator.
    pub fn new() -> Result<Self> {
        Ok(Seconds {
            current: SecondBucket::new(0)?,
            started: false,
        })
    }

    /// The bucket for wall-clock time `t_ns`, emitting the previous bucket if the second changed.
    pub fn bucket(&mut self, t_ns: u64, emit: &mut impl FnMut(Value)) -> &mut SecondBucket {
        let second = t_ns / NS_PER_S;
        if !self.started {
            self.started = true;
            self.current.reset(second);
        } else if second != self.current.second {
            emit(self.current.to_json());
            self.current.reset(second);
        }
        &mut self.current
    }

    /// Emits the last bucket, if any.
    pub fn finish(&mut self, emit: &mut impl FnMut(Value)) {
        if self.started {
            emit(self.current.to_json());
            self.started = false;
        }
    }
}

/// The writer's position, shared with reader processes through a small file in the control directory.
///
/// The writer's sampler thread publishes `next_seq` about every millisecond with one `pwrite` of 24 bytes:
/// position, flags (bit 0: writer finished, position is final) and a check word. Readers `pread` it; a torn read
/// fails the check and is retried.
#[derive(Debug)]
pub struct PositionFile {
    file: File,
}

/// A position read from a [`PositionFile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterPosition {
    /// The writer's next sequence number.
    pub next_seq: u64,
    /// Whether the writer has finished; `next_seq` is then the final record count.
    pub done: bool,
}

const POSITION_CHECK: u64 = 0x7474_7375_7374_6169;

impl PositionFile {
    /// Creates the file with position 0, not done.
    pub fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(path)?;
        let position = PositionFile { file };
        position.publish(0, false)?;
        Ok(position)
    }

    /// Opens an existing file for reading and publishing.
    pub fn open(path: &Path) -> Result<Self> {
        Ok(PositionFile {
            file: OpenOptions::new().read(true).write(true).open(path)?,
        })
    }

    /// Publishes a position.
    pub fn publish(&self, next_seq: u64, done: bool) -> Result<()> {
        let flags = u64::from(done);
        let mut bytes = [0; 24];
        bytes[..8].copy_from_slice(&next_seq.to_le_bytes());
        bytes[8..16].copy_from_slice(&flags.to_le_bytes());
        bytes[16..]
            .copy_from_slice(&(next_seq ^ flags.rotate_left(32) ^ POSITION_CHECK).to_le_bytes());
        self.file.write_all_at(&bytes, 0)?;
        Ok(())
    }

    /// Reads the latest position, retrying torn reads a few times.
    pub fn read(&self) -> Result<WriterPosition> {
        for _ in 0..16 {
            let mut bytes = [0; 24];
            self.file.read_exact_at(&mut bytes, 0)?;
            let word = |i: usize| u64::from_le_bytes(bytes[i..i + 8].try_into().unwrap_or([0; 8]));
            let (next_seq, flags, check) = (word(0), word(8), word(16));
            if check == next_seq ^ flags.rotate_left(32) ^ POSITION_CHECK && flags <= 1 {
                return Ok(WriterPosition {
                    next_seq,
                    done: flags == 1,
                });
            }
            std::thread::yield_now();
        }
        Err("writer position file stayed inconsistent".into())
    }
}

/// Process resource usage at one instant, from `getrusage`.
#[derive(Debug, Clone, Copy)]
pub struct CpuSnapshot {
    wall: Instant,
    user_us: i64,
    system_us: i64,
    minor_faults: i64,
    major_faults: i64,
    voluntary_switches: i64,
    involuntary_switches: i64,
}

impl CpuSnapshot {
    /// Usage of the whole process (all threads, including tickertape's helper threads and the sampler).
    pub fn process() -> Result<Self> {
        Self::take(nix::sys::resource::UsageWho::RUSAGE_SELF)
    }

    /// Usage of the calling thread only (Linux `RUSAGE_THREAD`); `None` on other systems.
    pub fn thread() -> Result<Option<Self>> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            Self::take(nix::sys::resource::UsageWho::RUSAGE_THREAD).map(Some)
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            Ok(None)
        }
    }

    // The libc field types (`time_t`, `suseconds_t`, `c_long`) differ between systems.
    #[allow(clippy::useless_conversion)]
    fn take(who: nix::sys::resource::UsageWho) -> Result<Self> {
        let usage = nix::sys::resource::getrusage(who)?;
        let micros =
            |t: nix::sys::time::TimeVal| i64::from(t.tv_sec()) * 1_000_000 + i64::from(t.tv_usec());
        Ok(CpuSnapshot {
            wall: Instant::now(),
            user_us: micros(usage.user_time()),
            system_us: micros(usage.system_time()),
            minor_faults: i64::from(usage.minor_page_faults()),
            major_faults: i64::from(usage.major_page_faults()),
            voluntary_switches: i64::from(usage.voluntary_context_switches()),
            involuntary_switches: i64::from(usage.involuntary_context_switches()),
        })
    }

    /// Usage between `self` and a later snapshot: CPU-seconds, fractions of wall time (1.0 is one core busy),
    /// page faults and context switches.
    pub fn delta_json(&self, later: &CpuSnapshot) -> Value {
        let wall = later.wall.duration_since(self.wall).as_secs_f64();
        let user = (later.user_us - self.user_us) as f64 / 1e6;
        let system = (later.system_us - self.system_us) as f64 / 1e6;
        let fraction = |cpu: f64| if wall > 0.0 { Some(cpu / wall) } else { None };
        json!({"wall_s": wall, "user_cpu_s": user, "system_cpu_s": system,
            "user_fraction": fraction(user), "system_fraction": fraction(system), "cpu_fraction": fraction(user + system),
            "minor_faults": later.minor_faults - self.minor_faults, "major_faults": later.major_faults - self.major_faults,
            "voluntary_context_switches": later.voluntary_switches - self.voluntary_switches,
            "involuntary_context_switches": later.involuntary_switches - self.involuntary_switches})
    }
}

/// Pins the calling thread to one CPU (Linux `sched_setaffinity`). Threads it starts later inherit the mask, so
/// pin after starting helper threads. On other systems this does nothing and says so.
pub fn pin_current_thread(cpu: usize) -> Result<String> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut set = nix::sched::CpuSet::new();
        set.set(cpu)?;
        nix::sched::sched_setaffinity(nix::unistd::Pid::from_raw(0), &set)
            .map_err(|e| format!("cannot pin to CPU {cpu}: {e}"))?;
        Ok(format!("pinned to CPU {cpu}"))
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        Ok(format!(
            "CPU pinning is not supported on this OS; requested CPU {cpu}, running unpinned"
        ))
    }
}

/// Parses a comma-separated CPU list such as `2,3,4`.
pub fn parse_cpu_list(text: &str) -> Result<Vec<usize>> {
    let cpus: Vec<usize> = text
        .split(',')
        .map(|c| c.parse().map_err(|_| format!("invalid CPU number {c:?}")))
        .collect::<std::result::Result<_, _>>()?;
    let mut sorted = cpus.clone();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.len() != cpus.len() {
        return Err("CPU list repeats a CPU".into());
    }
    Ok(cpus)
}

/// Bytes available to unprivileged users on the filesystem holding `path` (`statvfs`).
pub fn free_bytes(path: &Path) -> Result<u64> {
    let stat = nix::sys::statvfs::statvfs(path)?;
    #[allow(clippy::useless_conversion)] // The field types differ between systems.
    let bytes = u64::from(stat.blocks_available()).saturating_mul(u64::from(stat.fragment_size()));
    Ok(bytes)
}

/// Physical memory in bytes: `MemTotal` from `/proc/meminfo` on Linux, `sysctl hw.memsize` on macOS.
pub fn total_ram_bytes() -> Option<u64> {
    if cfg!(target_os = "macos") {
        let output = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        return String::from_utf8(output.stdout).ok()?.trim().parse().ok();
    }
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kib.checked_mul(1024)
}

/// A Unix second as an ISO 8601 UTC timestamp, e.g. `2026-11-19T08:30:00Z`.
pub fn utc_iso(unix_seconds: u64) -> String {
    let days = unix_seconds / 86_400;
    let secs = unix_seconds % 86_400;
    // Civil-from-days (Howard Hinnant's algorithm), valid for all dates after 1970.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3_600,
        secs / 60 % 60,
        secs % 60
    )
}

/// Appends JSON lines to a series file and flushes after each batch, so an interrupted run keeps its data.
#[derive(Debug)]
pub struct SeriesFile {
    out: std::io::BufWriter<File>,
}

impl SeriesFile {
    /// Creates a new file; refuses to overwrite.
    pub fn create(path: &Path) -> Result<Self> {
        Ok(SeriesFile {
            out: std::io::BufWriter::new(
                OpenOptions::new().create_new(true).write(true).open(path)?,
            ),
        })
    }

    /// Writes one line (not yet flushed).
    pub fn line(&mut self, value: &Value) -> Result<()> {
        serde_json::to_writer(&mut self.out, value)?;
        self.out.write_all(b"\n")?;
        Ok(())
    }

    /// Flushes buffered lines to the file.
    pub fn flush(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

/// Per-second fields of a writer `ops` line in the merged series, renamed from the per-process names.
const WRITER_OPS: [(&str, &str); 9] = [
    ("records", "records"),
    ("late_records", "late_records"),
    ("schedule_delay_max_us", "schedule_delay_max_us"),
    ("latency_max_us", "append_max_us"),
    ("latency_p99_us", "append_p99_us"),
    ("checkpoints", "checkpoints"),
    ("checkpoint_max_us", "checkpoint_max_us"),
    ("rolls", "rolls"),
    ("roll_max_us", "roll_max_us"),
];
/// Per-second fields of a reader `ops` line in the merged series.
const READER_OPS: [(&str, &str); 4] = [
    ("records", "records"),
    ("latency_max_us", "latency_max_us"),
    ("latency_p99_us", "latency_p99_us"),
    ("skipped_records", "skipped_records"),
];
/// Fields copied from `sample` lines (written by each process's sampler thread).
const SAMPLE_FIELDS: [&str; 9] = [
    "position",
    "writer_position",
    "lag_max_records",
    "lag_max_bytes",
    "cpu_fraction",
    "user_fraction",
    "system_fraction",
    "minor_faults",
    "major_faults",
];

/// Merges per-process series lines (index 0: writer, then readers in order) into one line per wall-clock second,
/// from the first to the last second seen: `{"t_unix_s", "t_utc", "writer": {...}, "readers": [{...}]}`.
///
/// Counts in a second with no line are 0 and maxima are null. If a process has two `ops` lines for the same
/// second (only after the wall clock stepped backwards), counts are added and maxima and p99s take the larger.
pub fn merge_series(processes: &[Vec<Value>]) -> Vec<Value> {
    use std::collections::BTreeMap;
    let mut seconds: BTreeMap<u64, Vec<serde_json::Map<String, Value>>> = BTreeMap::new();
    for (index, lines) in processes.iter().enumerate() {
        let ops: &[(&str, &str)] = if index == 0 { &WRITER_OPS } else { &READER_OPS };
        for line in lines {
            let Some(second) = line["second"].as_u64() else {
                continue;
            };
            let slot = &mut seconds
                .entry(second)
                .or_insert_with(|| vec![serde_json::Map::new(); processes.len()])[index];
            match line["kind"].as_str() {
                Some("ops") => {
                    for (from, to) in ops {
                        let value = &line[*from];
                        let merged = match (slot.get(*to), value) {
                            (Some(Value::Number(a)), Value::Number(b))
                                if !to.contains("max") && !to.contains("p99") =>
                            {
                                json!(a.as_f64().unwrap_or(0.0) + b.as_f64().unwrap_or(0.0))
                            }
                            (Some(Value::Number(a)), Value::Number(b)) => {
                                json!(a.as_f64().unwrap_or(0.0).max(b.as_f64().unwrap_or(0.0)))
                            }
                            (Some(existing), Value::Null) => existing.clone(),
                            _ => value.clone(),
                        };
                        slot.insert((*to).into(), merged);
                    }
                }
                Some("sample") => {
                    for field in SAMPLE_FIELDS {
                        if let Some(value) = line.get(field) {
                            slot.insert(field.into(), value.clone());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let (Some(&first), Some(&last)) = (seconds.keys().next(), seconds.keys().next_back()) else {
        return Vec::new();
    };
    let mut merged = Vec::new();
    for second in first..=last {
        let slots = seconds
            .remove(&second)
            .unwrap_or_else(|| vec![serde_json::Map::new(); processes.len()]);
        let filled: Vec<Value> = slots
            .into_iter()
            .enumerate()
            .map(|(index, mut slot)| {
                let ops: &[(&str, &str)] = if index == 0 { &WRITER_OPS } else { &READER_OPS };
                for (_, to) in ops {
                    let empty = if to.contains("max") || to.contains("p99") {
                        Value::Null
                    } else {
                        json!(0)
                    };
                    slot.entry(*to).or_insert(empty);
                }
                Value::Object(slot)
            })
            .collect();
        let mut processes = filled.into_iter();
        let writer = processes.next().unwrap_or(Value::Null);
        merged.push(
            json!({"t_unix_s": second, "t_utc": utc_iso(second), "writer": writer,
            "readers": processes.collect::<Vec<_>>()}),
        );
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_with_units_and_round_trip() {
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("600").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("20us").unwrap(), Duration::from_micros(20));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        for bad in ["", "m", "10x", "1.5s", "-1s", "99999999999999999999h"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_duration("750ns").unwrap(), Duration::from_nanos(750));
        for text in ["10m", "90s", "250ms", "20us", "2h", "0s", "750ns", "1500ns"] {
            assert_eq!(format_duration(parse_duration(text).unwrap()), text);
        }
    }

    #[test]
    fn steady_schedule_matches_tt_latency_and_counts_within_window() {
        let s = Schedule::steady(3).unwrap();
        for i in 0..10 {
            assert_eq!(
                s.intended_ns(100, i).unwrap(),
                crate::intended_ns(100, i, 3).unwrap()
            );
        }
        // Offsets 0, 333_333_333, 666_666_666 are inside one second; 1_000_000_000 is not.
        assert_eq!(s.count_within(1_000_000_000), Some(3));
        assert_eq!(s.count_within(1_000_000_001), Some(4));
        assert_eq!(s.count_within(0), Some(0));
        let fast = Schedule::steady(100_000).unwrap();
        assert_eq!(fast.count_within(600 * NS_PER_S), Some(60_000_000));
        assert!(Schedule::steady(0).is_err());
        assert!(s.intended_ns(u64::MAX, 1).is_err());
    }

    #[test]
    fn count_within_agrees_with_offsets_by_brute_force() {
        let schedules = [
            Schedule::steady(7).unwrap(),
            Schedule::steady(1_000).unwrap(),
            Schedule::bursty(1_000, Duration::from_millis(100), 10).unwrap(),
            Schedule::bursty(333, Duration::from_millis(70), 50).unwrap(),
            Schedule::bursty(10, Duration::from_secs(1), 100).unwrap(),
        ];
        for schedule in schedules {
            for window in [
                0,
                1,
                999_999,
                1_000_000,
                33_000_000,
                100_000_000,
                250_000_001,
                1_000_000_000,
            ] {
                let brute = (0..)
                    .take_while(|&i| schedule.offset_ns(i).unwrap().unwrap() < window)
                    .count() as u64;
                assert_eq!(
                    schedule.count_within(window),
                    Some(brute),
                    "{schedule:?} {window}"
                );
            }
        }
    }

    #[test]
    fn bursty_schedule_sends_bursts_at_peak_rate_then_pauses() {
        // 1,000,000 records/s for 10 % of every 100 ms: bursts of 10,000 records over 10 ms.
        let s = Schedule::bursty(1_000_000, Duration::from_millis(100), 10).unwrap();
        let Schedule::Bursty {
            burst_records,
            on_ns,
            ..
        } = s
        else {
            panic!("not bursty")
        };
        assert_eq!((burst_records, on_ns), (10_000, 10_000_000));
        assert_eq!(s.offset_ns(0).unwrap(), Some(0));
        assert_eq!(s.offset_ns(1).unwrap(), Some(1_000));
        assert_eq!(s.offset_ns(9_999).unwrap(), Some(9_999_000));
        // The next burst starts exactly one period later; nothing is scheduled in the off part.
        assert_eq!(s.offset_ns(10_000).unwrap(), Some(100_000_000));
        assert_eq!(s.offset_ns(25_000).unwrap(), Some(205_000_000));
        assert_eq!(s.mean_rate(), Some(100_000.0));
        assert_eq!(s.count_within(NS_PER_S), Some(100_000));
        // Offsets never decrease, even across bursts.
        let mut last = 0;
        for i in 0..50_000 {
            let offset = s.offset_ns(i).unwrap().unwrap();
            assert!(offset >= last);
            last = offset;
        }
        assert!(Schedule::bursty(1, Duration::from_millis(1), 10).is_err());
        assert!(Schedule::bursty(1_000, Duration::from_secs(1), 0).is_err());
        assert!(Schedule::bursty(1_000, Duration::from_secs(1), 101).is_err());
        assert!(Schedule::bursty(0, Duration::from_secs(1), 10).is_err());
    }

    #[test]
    fn unpaced_schedule_has_no_intended_times() {
        let s = Schedule::Unpaced;
        assert_eq!(s.offset_ns(5).unwrap(), None);
        assert!(s.intended_ns(0, 5).is_err());
        assert_eq!(s.count_within(NS_PER_S), None);
        assert_eq!(s.mean_rate(), None);
    }

    #[test]
    fn reader_specs_parse_and_round_trip() {
        let specs = ReaderSpec::parse_list(
            "mapped,safe:durable:wait=block,safe:delay=20us,mapped:stall=200ms/5s:wait=sleep",
            WaitMode::Spin,
        )
        .unwrap();
        assert_eq!(specs.len(), 4);
        assert!(specs[0].mapped && !specs[0].durable_only && specs[0].wait == WaitMode::Spin);
        assert!(!specs[1].mapped && specs[1].durable_only && specs[1].wait == WaitMode::Block);
        assert_eq!(specs[2].delay, Some(Duration::from_micros(20)));
        assert!(specs[2].lagging() && !specs[0].lagging());
        assert_eq!(
            specs[3].stall,
            Some(Stall {
                pause: Duration::from_millis(200),
                every: Duration::from_secs(5)
            })
        );
        for spec in &specs {
            assert_eq!(
                &ReaderSpec::parse(&spec.label(), WaitMode::Default).unwrap(),
                spec
            );
        }
        assert!(specs[1].reader_config().durable_only);
        assert!(
            ReaderSpec::parse_list("none", WaitMode::Spin)
                .unwrap()
                .is_empty()
        );
        for bad in [
            "",
            "fast",
            "safe:wat",
            "safe:durable:durable",
            "safe:wait=nap",
            "safe:delay=0us",
            "safe:stall=5s/1s",
            "safe:stall=1s",
            "safe,",
        ] {
            assert!(
                ReaderSpec::parse_list(bad, WaitMode::Spin).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn lag_is_never_negative_and_reported_in_records_and_bytes() {
        assert_eq!(lag_records(10, 4), 6);
        assert_eq!(lag_records(4, 10), 0);
        assert_eq!(record_bytes(64).unwrap(), 104);
        assert_eq!(record_bytes(1024).unwrap(), 1064);
        assert_eq!(record_bytes(1).unwrap(), 48);
        let mut hist = Histogram::<u64>::new(3).unwrap();
        let empty = lag_json(&hist, 104);
        assert_eq!(empty["samples"], 0);
        assert_eq!(empty["max_records"], Value::Null);
        for lag in [0, 0, 0, 10, 1_000] {
            hist.record(lag).unwrap();
        }
        let lag = lag_json(&hist, 104);
        assert_eq!(lag["samples"], 5);
        assert_eq!(lag["min_records"], 0.0);
        assert_eq!(lag["p50_records"], 0.0);
        assert_eq!(lag["max_records"], 1_000.0);
        assert_eq!(lag["max_bytes"], 104_000.0);
        assert!(lag.get("count").is_none());
    }

    #[test]
    fn quantiles_use_unit_suffixes_and_order() {
        let mut hist = Histogram::<u64>::new(3).unwrap();
        for v in 1..=10_000 {
            hist.record(v * 1000).unwrap();
        }
        let q = quantiles_json(&hist, "us", 1000.0);
        let keys = [
            "min_us",
            "p50_us",
            "p90_us",
            "p99_us",
            "p99_9_us",
            "p99_99_us",
            "max_us",
        ];
        let values: Vec<f64> = keys.iter().map(|k| q[*k].as_f64().unwrap()).collect();
        assert!(values.windows(2).all(|w| w[0] <= w[1]), "{values:?}");
        assert_eq!(q["count"], 10_000);
        assert_eq!(values[0], 1.0);
        assert!((values[1] - 5_000.0).abs() / 5_000.0 < 0.001);
        assert!((values[3] - 9_900.0).abs() / 9_900.0 < 0.001);
        assert_eq!(q, crate::histogram_json(&hist));
    }

    #[test]
    fn seconds_emit_one_bucket_per_second_and_reuse_storage() {
        let mut seconds = Seconds::new().unwrap();
        let mut lines = Vec::new();
        let mut emit = |v: Value| lines.push(v);
        for (t, latency) in [
            (5_100_000_000u64, 1_000u64),
            (5_900_000_000, 2_000),
            (6_000_000_000, 700),
        ] {
            let bucket = seconds.bucket(t, &mut emit);
            bucket.records += 1;
            bucket.latency.record(latency).unwrap();
        }
        seconds.bucket(8_500_000_000, &mut emit).checkpoints += 1;
        seconds.finish(&mut emit);
        seconds.finish(&mut emit);
        let summary: Vec<_> = lines
            .iter()
            .map(|l| {
                (
                    l["second"].as_u64().unwrap(),
                    l["records"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(summary, [(5, 2), (6, 1), (8, 0)]);
        assert_eq!(lines[0]["latency_max_us"], 2.0);
        assert_eq!(lines[2]["latency_max_us"], Value::Null);
        assert_eq!(lines[2]["checkpoints"], 1);
    }

    #[test]
    fn position_file_round_trips_and_rejects_torn_words() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer.pos");
        let writer = PositionFile::create(&path).unwrap();
        let reader = PositionFile::open(&path).unwrap();
        assert_eq!(
            reader.read().unwrap(),
            WriterPosition {
                next_seq: 0,
                done: false
            }
        );
        writer.publish(123_456, false).unwrap();
        assert_eq!(reader.read().unwrap().next_seq, 123_456);
        writer.publish(u64::MAX - 1, true).unwrap();
        assert_eq!(
            reader.read().unwrap(),
            WriterPosition {
                next_seq: u64::MAX - 1,
                done: true
            }
        );
        // A half-updated file (new position, old check word) is never accepted.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .write_all_at(&7u64.to_le_bytes(), 0)
            .unwrap();
        assert!(reader.read().is_err());
        assert!(PositionFile::create(&path).is_err());
    }

    #[test]
    fn cpu_snapshots_measure_this_process() {
        let start = CpuSnapshot::process().unwrap();
        let mut x = 0u64;
        let until = Instant::now() + Duration::from_millis(30);
        while Instant::now() < until {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        let usage = start.delta_json(&CpuSnapshot::process().unwrap());
        assert!(usage["wall_s"].as_f64().unwrap() >= 0.03);
        assert!(
            usage["user_cpu_s"].as_f64().unwrap() + usage["system_cpu_s"].as_f64().unwrap() > 0.0
        );
        assert!(usage["minor_faults"].as_i64().unwrap() >= 0);
        assert_eq!(
            CpuSnapshot::thread().unwrap().is_some(),
            cfg!(target_os = "linux")
        );
    }

    #[test]
    fn host_facts_and_pinning() {
        assert!(total_ram_bytes().unwrap() > 0);
        assert!(free_bytes(Path::new(".")).unwrap() > 0);
        assert_eq!(parse_cpu_list("0,2,1").unwrap(), [0, 2, 1]);
        assert!(parse_cpu_list("0,0").is_err() && parse_cpu_list("a").is_err());
        let message = pin_current_thread(0).unwrap();
        if cfg!(target_os = "linux") {
            assert_eq!(message, "pinned to CPU 0");
        } else {
            assert!(message.contains("not supported"));
        }
    }

    #[test]
    fn utc_timestamps() {
        assert_eq!(utc_iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_iso(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc_iso(1_795_084_201), "2026-11-19T10:30:01Z");
        assert_eq!(utc_iso(4_102_444_799), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn series_merge_fills_gaps_and_combines_duplicates() {
        let writer = vec![
            json!({"kind": "ops", "second": 10, "records": 5, "late_records": 1, "schedule_delay_max_us": 3.0,
                "latency_max_us": 9.0, "latency_p99_us": 8.0, "checkpoints": 0, "checkpoint_max_us": 0.0,
                "rolls": 0, "roll_max_us": 0.0, "skipped_records": 0}),
            json!({"kind": "sample", "second": 10, "position": 5, "cpu_fraction": 1.0}),
            // A second line for second 10 after a clock step: counts add, maxima take the larger.
            json!({"kind": "ops", "second": 10, "records": 2, "late_records": 0, "schedule_delay_max_us": 1.0,
                "latency_max_us": 20.0, "latency_p99_us": 4.0, "checkpoints": 1, "checkpoint_max_us": 500.0,
                "rolls": 0, "roll_max_us": 0.0, "skipped_records": 0}),
            json!({"kind": "ops", "second": 12, "records": 1, "late_records": 0, "schedule_delay_max_us": 0.0,
                "latency_max_us": 1.0, "latency_p99_us": 1.0, "checkpoints": 0, "checkpoint_max_us": 0.0,
                "rolls": 1, "roll_max_us": 900.0, "skipped_records": 0}),
        ];
        let reader = vec![
            json!({"kind": "ops", "second": 11, "records": 7, "latency_max_us": null, "latency_p99_us": null,
                "skipped_records": 3, "late_records": 0}),
            json!({"kind": "sample", "second": 11, "lag_max_records": 40, "lag_max_bytes": 4160}),
        ];
        let merged = merge_series(&[writer, reader]);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0]["t_unix_s"], 10);
        assert_eq!(merged[0]["t_utc"], "1970-01-01T00:00:10Z");
        let w = &merged[0]["writer"];
        assert_eq!(w["records"], 7.0);
        assert_eq!(w["late_records"], 1.0);
        assert_eq!(w["append_max_us"], 20.0);
        assert_eq!(w["append_p99_us"], 8.0);
        assert_eq!(w["checkpoint_max_us"], 500.0);
        assert_eq!(w["position"], 5);
        assert!(w.get("latency_max_us").is_none() && w.get("skipped_records").is_none());
        // Second 11 has no writer line: zero counts, null maxima.
        assert_eq!(merged[1]["writer"]["records"], 0);
        assert_eq!(merged[1]["writer"]["append_max_us"], Value::Null);
        let r = &merged[1]["readers"][0];
        assert_eq!(
            (r["records"].clone(), r["skipped_records"].clone()),
            (json!(7), json!(3))
        );
        assert_eq!(r["lag_max_records"], 40);
        assert!(r.get("late_records").is_none());
        assert_eq!(merged[2]["readers"][0]["records"], 0);
        assert_eq!(merged[2]["writer"]["rolls"], 1);
        assert!(merge_series(&[Vec::new()]).is_empty());
    }
}
