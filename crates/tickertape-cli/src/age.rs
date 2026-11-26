//! `tickertape age`: follows a journal from its end and reports how old its records are when this command's reader
//! returns them (ADR 0047).

use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use clap::{Args, ValueEnum};
use serde::Serialize;
use tickertape::latency::{RecordAge, RecordAgeSnapshot, Snapshot};
use tickertape::{Backoff, Error, Reader, ReaderConfig};

use crate::{Failure, Result, output, status, tail};

/// Version of the JSON object; bumped when a field is removed or changes meaning (adding fields doesn't bump it).
const SCHEMA_VERSION: u32 = 1;
/// The interval when none of `--duration`, `--count` and `--interval` is given.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);
/// Reports that may wait for a slow standard output before interval reports are skipped.
const PENDING_REPORTS: usize = 4;

pub(crate) const HELP: &str = "Follows the journal from its end with the safe reader and reports record ages: this machine's wall clock when the command's reader returned a record, minus the record's local_ts_ns, which whoever appended it supplied. It prints no records. It measures THIS COMMAND'S READER on the CPU it happens to run on, not your application's reader: it says whether the writer, the clock and the machine are healthy. An application measures its own reader with tickertape::latency::RecordAge. Age is not the writer-to-reader latency, unless the writer stamps local_ts_ns right before it appends.\n\nIt starts at the end of the journal, as tail --from-end (Reader::seek_to_end: reads the newest segment from the checkpoint on), and every record from there on counts (live), except after discarded history, until it has caught up again. A report with live false and records 0 measured nothing: the reader was not at the end of the journal yet. Records without a local_ts_ns, or with one later than this machine's clock, are counted apart; if any were ahead of the clock, the minimum and the percentiles are biased upwards.\n\n--wait poll (default) and spin make no system call in the writer's appends: they never register as a waiter. poll sleeps 10 us, doubling to 1 ms while idle, and those sleeps are in the ages; spin has no such floor and uses one core. block waits in the kernel on the writer's wake-up region: an append that finds it waiting makes a wake-up system call before it returns; with --durable-only it waits for checkpoints instead, and only a checkpoint makes that call (from 1.1; beside a 1.0.0 writer it polls every 1 ms).\n\nEnds --duration after its start or after --count records returned while live, whichever comes first, with a final report; --interval prints reports before that. With none of the three it reports every 10s until interrupted; an interrupt ends it without a final report.\n\nJSON schema (schema_version 1; fields may be added; one object per report and line): {\"schema_version\":1,\"journal\":string,\"sampled_unix_ns\":u64,\"elapsed_s\":number,\"final\":bool,\"live\":bool,\"durable_only\":bool,\"wait\":\"poll\"|\"spin\"|\"block\",\"records\":u64,\"first_seq\":u64|null,\"last_seq\":u64|null,\"before_live\":u64,\"after_discontinuity\":u64,\"unset_timestamp\":u64,\"ahead_of_clock\":u64,\"discontinuities\":u64,\"age\":HIST,\"interval_age\":HIST|null} with HIST = {\"count\":u64,\"min_ns\":u64|null,\"mean_ns\":number|null,\"p50_ns\":u64|null,\"p90_ns\":u64|null,\"p99_ns\":u64|null,\"p99_9_ns\":u64|null,\"p99_99_ns\":u64|null,\"max_ns\":u64|null}; see docs/cli.md.\n\nExit codes: 0 success; 1 usage, I/O or missing journal; 2 corruption or unsupported journal format (an error while following comes after the report of what was measured; a journal that can't be opened, or whose end can't be reached, gets no final report).";

#[derive(Debug, Args)]
pub(crate) struct AgeArgs {
    /// Journal directory (opened read-only; no writer lock)
    dir: PathBuf,
    /// Stop DUR after the command's start (integer followed by ms, s, m or h)
    #[arg(long, value_name = "DUR", value_parser = parse_dur)]
    duration: Option<Duration>,
    /// Stop after N records returned while live
    #[arg(long, value_name = "N")]
    count: Option<u64>,
    /// Report every DUR (integer followed by ms, s, m or h)
    #[arg(long, value_name = "DUR", value_parser = parse_dur)]
    interval: Option<Duration>,
    /// Return only checkpointed records: the age at which a durable-only reader gets them
    #[arg(long)]
    durable_only: bool,
    /// How to wait for records: poll (sleeps of 10 us to 1 ms are in the ages), spin (one core), or block (an append that finds this command waiting pays a wake-up system call)
    #[arg(long, value_enum, default_value = "poll")]
    wait: Wait,
    /// Emit one JSON object per report and line instead of text (schema below)
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
enum Wait {
    Poll,
    Spin,
    Block,
}

impl Wait {
    /// Only `Block` may register as a waiter: what watches a writer must not make its appends slower.
    fn reader_config(self) -> ReaderConfig {
        let config = ReaderConfig::default();
        match self {
            Wait::Poll => config.notify(false),
            Wait::Spin => config.notify(false).backoff(Backoff::busy_spin()),
            Wait::Block => config.notify(true),
        }
    }
}

fn parse_dur(value: &str) -> std::result::Result<Duration, String> {
    let duration = status::parse_interval(value)?;
    if Instant::now().checked_add(duration).is_none() {
        return Err("duration exceeds the platform's clock range".into());
    }
    Ok(duration)
}

/// What the reading thread hands over for one report.
struct Report {
    sampled_unix_ns: u64,
    elapsed: Duration,
    last: bool,
    live: bool,
    records: u64,
    seqs: Option<(u64, u64)>,
    discontinuities: u64,
    ages: RecordAgeSnapshot,
}

impl Report {
    /// While the reader is not at the end of the journal yet.
    fn nothing(start: Instant, last: bool) -> Report {
        Report {
            sampled_unix_ns: tickertape::clock::realtime_ns(),
            elapsed: start.elapsed(),
            last,
            live: false,
            records: 0,
            seqs: None,
            discontinuities: 0,
            ages: RecordAge::new().view().snapshot(),
        }
    }
}

pub(crate) fn run(args: AgeArgs, out: &mut dyn Write) -> Result<()> {
    let start = Instant::now();
    let interval = match (args.interval, args.duration, args.count) {
        (None, None, None) => Some(DEFAULT_INTERVAL),
        (interval, _, _) => interval,
    };
    let reader = Reader::open(
        &args.dir,
        args.wait.reader_config().durable_only(args.durable_only),
    )?;
    let journal = fs::canonicalize(&args.dir)
        .map_err(|e| output::io_error("resolve journal directory", e))?;
    let mut printer = Printer {
        journal: &journal,
        durable_only: args.durable_only,
        wait: args.wait,
        json: args.json,
        interval: interval.is_some(),
        previous: None,
    };
    let deadline = args.duration.and_then(|d| start.checked_add(d));
    let mut next_report = interval.and_then(|i| start.checked_add(i));
    let placed = place(
        move || {
            let mut reader = reader;
            reader.seek_to_end()?;
            Ok(reader)
        },
        deadline,
        &mut next_report,
        interval,
        |last| printer.print(&Report::nothing(start, last), out),
    )?;
    let Some(reader) = placed else {
        return Ok(()); // `--duration` passed first.
    };
    // Reports are formatted and written here, and the reader runs on its own thread: the time a report takes (a slow
    // terminal, a full pipe) must not be in the ages. If a report can't be written, this function returns at once and
    // the process ends, with the reader wherever it is.
    let (sender, reports) = mpsc::sync_channel(PENDING_REPORTS);
    let follower = Follower {
        reader,
        age: RecordAge::new(),
        live: false,
        records: 0,
        seqs: None,
        discontinuities: 0,
        start,
        deadline,
        count: args.count,
        next_report,
        interval,
        reports: sender,
    };
    let reading = std::thread::Builder::new()
        .name("age-reader".into())
        .spawn(move || follower.run())
        .map_err(|e| output::io_error("start the reading thread", e))?;
    for report in reports {
        printer.print(&report, out)?;
    }
    reading
        .join()
        .unwrap_or_else(|_| Err(Failure::usage("the reading thread panicked")))
}

/// Runs `seek` (which places the reader at the end of the journal) on a thread of its own, and calls `report` (with
/// whether it is the last) for the interval reports that fall due meanwhile, and at `deadline`.
///
/// `Ok(None)` if the deadline passed first: after the last report, with the thread still seeking. A seek can't be given a
/// deadline, and against a writer that commits as fast as the reader validates, it lasts for as long as the writer does;
/// `--duration` must end the command all the same. It usually takes milliseconds.
fn place<T: Send + 'static>(
    seek: impl FnOnce() -> Result<T> + Send + 'static,
    deadline: Option<Instant>,
    next_report: &mut Option<Instant>,
    interval: Option<Duration>,
    mut report: impl FnMut(bool) -> Result<()>,
) -> Result<Option<T>> {
    let (sender, placed) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("age-seek".into())
        .spawn(move || {
            // The receiver is gone if the deadline passed or a report could not be written.
            let _ = sender.send(seek());
        })
        .map_err(|e| output::io_error("start the seeking thread", e))?;
    loop {
        let until = [deadline, *next_report].into_iter().flatten().min();
        let received = match until {
            None => placed.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(t) => placed.recv_timeout(t.saturating_duration_since(Instant::now())),
        };
        match received {
            Ok(result) => return result.map(Some),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(Failure::usage("the seeking thread panicked"));
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        let now = Instant::now();
        if deadline.is_some_and(|d| now >= d) {
            report(true)?;
            return Ok(None);
        }
        if next_report.is_some_and(|due| now >= due) {
            report(false)?;
            *next_report = following(*next_report, interval, Instant::now());
        }
    }
}

/// When the report after the one that was due at `due` is due: keeps the phase, unless a whole interval was missed.
fn following(due: Option<Instant>, interval: Option<Duration>, now: Instant) -> Option<Instant> {
    let interval = interval?;
    due.and_then(|due| due.checked_add(interval))
        .filter(|next| *next > now)
        .or_else(|| now.checked_add(interval))
}

/// What has been read since the clock was.
#[derive(Default)]
struct Unchecked {
    records: u32,
    bytes: usize,
}

impl Unchecked {
    /// Adds a record. Whether the clock is due: after 1,024 records, or fewer large ones, so that a deadline or a report
    /// is late by about as little whatever the records' size.
    fn add(&mut self, payload_len: usize) -> bool {
        self.records += 1;
        self.bytes = self.bytes.saturating_add(payload_len);
        self.records >= 1024 || self.bytes >= 1 << 20
    }
}

/// Follows the journal and counts ages.
struct Follower {
    reader: Reader,
    age: RecordAge,
    live: bool,
    /// Records returned while live, and the first and last sequence number among them.
    records: u64,
    seqs: Option<(u64, u64)>,
    discontinuities: u64,
    start: Instant,
    deadline: Option<Instant>,
    count: Option<u64>,
    next_report: Option<Instant>,
    interval: Option<Duration>,
    reports: SyncSender<Report>,
}

impl Follower {
    /// From a reader at the end of the journal, so live from the first record on. Ends with a last report, also after
    /// an error (which follows it).
    fn run(mut self) -> Result<()> {
        self.set_live(true);
        let result = self.follow();
        // Waits for room, unlike an interval report. The receiver is gone only if a report could not be written, which
        // is what the command then fails with.
        let _ = self.reports.send(self.report(true));
        result
    }

    fn follow(&mut self) -> Result<()> {
        // Whether the last read found nothing, so that the next one waits.
        let mut wait = false;
        let mut unchecked = Unchecked::default();
        // The clock is read before every wait, and otherwise only now and then: neither a backlog nor the
        // history read after a discontinuity should pay for a clock read per record.
        let mut check_clock = true;
        loop {
            if self.count.is_some_and(|n| self.records >= n) {
                return Ok(());
            }
            let mut timeout = None;
            if wait || check_clock {
                let now = Instant::now();
                if self.deadline.is_some_and(|d| now >= d) {
                    return Ok(());
                }
                if self.next_report.is_some_and(|due| now >= due) {
                    match self.reports.try_send(self.report(false)) {
                        // Full: standard output is behind. Reports are cumulative, and the next one's interval starts at
                        // the last report that was printed, so nothing is lost by skipping this one.
                        Ok(()) | Err(TrySendError::Full(_)) => {}
                        Err(TrySendError::Disconnected(_)) => return Ok(()),
                    }
                    self.next_report = following(self.next_report, self.interval, now);
                }
                let until = [self.deadline, self.next_report]
                    .into_iter()
                    .flatten()
                    .min();
                timeout = until.map(|t| t.saturating_duration_since(now));
                unchecked = Unchecked::default();
                check_clock = false;
            }
            let next = if wait {
                self.reader.next_timeout(timeout)
            } else {
                self.reader.try_next()
            };
            match next {
                Ok(Some(record)) => {
                    wait = false;
                    check_clock = unchecked.add(record.payload.len());
                    if self.live {
                        self.age.observe(&record);
                        self.records += 1;
                        let first = self.seqs.map_or(record.seq, |(first, _)| first);
                        self.seqs = Some((first, record.seq));
                    } else {
                        // Only counted, as not live: what the time is makes no difference.
                        self.age.observe_at(record.local_ts_ns, 0);
                    }
                }
                Ok(None) => {
                    // Nothing more to read: caught up, so every record from here on is a new one.
                    self.set_live(true);
                    wait = true;
                }
                Err(Error::Discarded { seq, .. }) => {
                    self.set_live(false);
                    wait = false;
                    check_clock = true;
                    tail::resume_as("age", &mut self.reader, seq)?;
                    self.discontinuities += 1;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn set_live(&mut self, live: bool) {
        self.live = live;
        self.age.set_live(live);
    }

    fn report(&self, last: bool) -> Report {
        Report {
            sampled_unix_ns: tickertape::clock::realtime_ns(),
            elapsed: self.start.elapsed(),
            last,
            live: self.live,
            records: self.records,
            seqs: self.seqs,
            discontinuities: self.discontinuities,
            ages: self.age.view().snapshot(),
        }
    }
}

struct Printer<'a> {
    journal: &'a Path,
    durable_only: bool,
    wait: Wait,
    json: bool,
    /// Whether reports have an interval.
    interval: bool,
    previous: Option<Snapshot>,
}

impl Printer<'_> {
    fn print(&mut self, report: &Report, out: &mut dyn Write) -> Result<()> {
        let interval_age = self.interval.then(|| match &self.previous {
            Some(previous) => report.ages.age.since(previous),
            None => report.ages.age.clone(),
        });
        let json = JsonReport {
            schema_version: SCHEMA_VERSION,
            journal: self.journal.to_string_lossy().into_owned(),
            sampled_unix_ns: report.sampled_unix_ns,
            elapsed_s: report.elapsed.as_secs_f64(),
            last: report.last,
            live: report.live,
            durable_only: self.durable_only,
            wait: self.wait,
            records: report.records,
            first_seq: report.seqs.map(|(first, _)| first),
            last_seq: report.seqs.map(|(_, last)| last),
            before_live: report.ages.before_live,
            after_discontinuity: report.ages.after_discontinuity,
            unset_timestamp: report.ages.unset,
            ahead_of_clock: report.ages.ahead,
            discontinuities: report.discontinuities,
            age: Hist::new(&report.ages.age),
            interval_age: interval_age.as_ref().map(Hist::new),
        };
        if self.json {
            output::json(out, &json)?;
        } else {
            let mut text = String::new();
            if self.previous.is_some() {
                text.push('\n');
            }
            render_text(&mut text, &json);
            out.write_all(text.as_bytes())
                .map_err(|e| output::io_error("write age output", e))?;
        }
        out.flush()
            .map_err(|e| output::io_error("flush age output", e))?;
        self.previous = Some(report.ages.age.clone());
        Ok(())
    }
}

#[derive(Serialize)]
struct JsonReport {
    schema_version: u32,
    journal: String,
    sampled_unix_ns: u64,
    elapsed_s: f64,
    #[serde(rename = "final")]
    last: bool,
    live: bool,
    durable_only: bool,
    wait: Wait,
    records: u64,
    first_seq: Option<u64>,
    last_seq: Option<u64>,
    before_live: u64,
    after_discontinuity: u64,
    unset_timestamp: u64,
    ahead_of_clock: u64,
    discontinuities: u64,
    age: Hist,
    interval_age: Option<Hist>,
}

#[derive(Serialize)]
struct Hist {
    count: u64,
    min_ns: Option<u64>,
    mean_ns: Option<f64>,
    p50_ns: Option<u64>,
    p90_ns: Option<u64>,
    p99_ns: Option<u64>,
    p99_9_ns: Option<u64>,
    p99_99_ns: Option<u64>,
    max_ns: Option<u64>,
}

impl Hist {
    fn new(s: &Snapshot) -> Hist {
        let ns = |d: Option<Duration>| d.and_then(|d| u64::try_from(d.as_nanos()).ok());
        let quantile = |q| ns(s.value_at_quantile(q));
        Hist {
            count: s.count(),
            min_ns: ns(s.min()),
            mean_ns: ns(s.sum())
                .filter(|_| s.count() > 0)
                .map(|sum| sum as f64 / s.count() as f64),
            p50_ns: quantile(0.5),
            p90_ns: quantile(0.9),
            p99_ns: quantile(0.99),
            p99_9_ns: quantile(0.999),
            p99_99_ns: quantile(0.9999),
            max_ns: ns(s.max()),
        }
    }
}

fn render_text(text: &mut String, r: &JsonReport) {
    let seqs = match (r.first_seq, r.last_seq) {
        (Some(first), Some(last)) => format!("  seq {first}..={last}"),
        _ => String::new(),
    };
    let wait = match r.wait {
        Wait::Poll => "poll",
        Wait::Spin => "spin",
        Wait::Block => "block",
    };
    // Writing to a `String` can't fail.
    let _ = writeln!(text, "journal     {}", r.journal);
    let _ = writeln!(
        text,
        "elapsed     {:.3} s  wait={wait}{}{}",
        r.elapsed_s,
        if r.durable_only { "  durable-only" } else { "" },
        if r.last { "  final" } else { "" }
    );
    let _ = writeln!(text, "records     {}{seqs}", r.records);
    let _ = writeln!(
        text,
        "not counted unset_timestamp={} ahead_of_clock={} before_live={} after_discontinuity={} discontinuities={}",
        r.unset_timestamp,
        r.ahead_of_clock,
        r.before_live,
        r.after_discontinuity,
        r.discontinuities
    );
    render_hist(text, "age", &r.age);
    if let Some(interval) = &r.interval_age {
        render_hist(text, "interval", interval);
    }
    if !r.live {
        let _ = writeln!(
            text,
            "note        {}",
            if r.records == 0 {
                "not live yet: nothing measured"
            } else {
                "not live: the records found after a discontinuity are not all read, and are not measured"
            }
        );
    }
    if r.ahead_of_clock > 0 {
        let _ = writeln!(
            text,
            "note        timestamps were ahead of this machine's clock: the minimum and the percentiles are biased \
             upwards"
        );
    }
}

fn render_hist(text: &mut String, label: &str, h: &Hist) {
    let ns = |v: Option<u64>| v.map_or_else(|| "-".to_string(), |v| micros(v as f64));
    let _ = writeln!(
        text,
        "{label:<11} count={} min={} mean={} p50={} p90={} p99={} p99.9={} p99.99={} max={}",
        h.count,
        ns(h.min_ns),
        h.mean_ns.map_or_else(|| "-".to_string(), micros),
        ns(h.p50_ns),
        ns(h.p90_ns),
        ns(h.p99_ns),
        ns(h.p99_9_ns),
        ns(h.p99_99_ns),
        ns(h.max_ns)
    );
}

/// Nanoseconds in microseconds, always the same unit so that columns and reports compare at a glance.
fn micros(ns: f64) -> String {
    format!("{:.3}us", ns / 1e3)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `poll` and `spin` must never register as a waiter in the writer's wake-up region: a reader with `notify(false)`
    /// has no handle to it. (`tests/age.rs` checks the running command on Linux.)
    #[test]
    fn only_block_may_wait_on_the_wake_up_region() {
        assert!(!Wait::Poll.reader_config().notify);
        assert!(!Wait::Spin.reader_config().notify);
        assert!(Wait::Block.reader_config().notify);
        assert_eq!(Wait::Poll.reader_config().backoff, Backoff::default());
        assert_eq!(Wait::Spin.reader_config().backoff, Backoff::busy_spin());
        assert_eq!(Wait::Block.reader_config(), ReaderConfig::default());
    }

    #[test]
    fn durations_as_in_status_watch() {
        assert_eq!(parse_dur("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(parse_dur("2h"), Ok(Duration::from_secs(7200)));
        for bad in [
            "0s",
            "1d",
            "5",
            "-1s",
            "ms",
            "1.5s",
            "18446744073709551615s",
        ] {
            assert!(parse_dur(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn saturated_and_empty_histograms_have_nulls() {
        let mut age = RecordAge::new();
        age.set_live(true);
        let empty = Hist::new(&age.view().snapshot().age);
        assert_eq!((empty.count, empty.max_ns, empty.mean_ns), (0, None, None));
        for _ in 0..11 {
            age.observe_at(1_000, 1_800_000_000_000_001_000);
        }
        let saturated = Hist::new(&age.view().snapshot().age);
        assert_eq!((saturated.count, saturated.mean_ns), (11, None));
        assert_eq!(saturated.max_ns, Some(1_800_000_000_000_000_000));
        let mut text = String::new();
        render_hist(&mut text, "age", &saturated);
        assert!(text.contains(" mean=- "), "{text}");
    }

    /// Runs the command in this process; its standard output.
    fn run_args(dir: &Path, json: bool, set: impl FnOnce(&mut AgeArgs)) -> String {
        let mut args = AgeArgs {
            dir: dir.into(),
            duration: None,
            count: None,
            interval: None,
            durable_only: false,
            wait: Wait::Poll,
            json,
        };
        set(&mut args);
        let mut out = Vec::new();
        run(args, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn journal(records: usize, payload: &[u8]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = tickertape::Writer::open(
            dir.path(),
            tickertape::WriterConfig::default().segment_size(4 << 20),
        )
        .unwrap();
        for _ in 0..records {
            writer
                .append(&tickertape::NewRecord {
                    schema_id: 1,
                    venue_ts_ns: 0,
                    local_ts_ns: 1,
                    payload,
                })
                .unwrap();
        }
        dir
    }

    /// A reader that can't be placed at the end before `--duration` has passed measured nothing, and the reports say so:
    /// they must not look like an idle journal's. The command ends on time all the same.
    #[test]
    fn duration_and_interval_hold_while_the_reader_is_being_placed() {
        let start = Instant::now();
        let after = |ms| start.checked_add(Duration::from_millis(ms));
        let mut out = Vec::new();
        let mut printer = Printer {
            journal: Path::new("/j"),
            durable_only: false,
            wait: Wait::Poll,
            json: true,
            interval: true,
            previous: None,
        };
        // Never sent: the seek does not end while the test runs.
        let (hold, held) = mpsc::channel::<()>();
        let mut next_report = after(10);
        let placed = place(
            move || {
                let _ = held.recv();
                Ok(())
            },
            after(200),
            &mut next_report,
            Some(Duration::from_millis(10)),
            |last| printer.print(&Report::nothing(start, last), &mut out),
        );
        assert!(matches!(placed, Ok(None)));
        assert!(start.elapsed() >= Duration::from_millis(200));
        drop(hold);
        let reports: Vec<serde_json::Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let (last, earlier) = reports.split_last().unwrap();
        assert_eq!(last["final"], true);
        assert!(!earlier.is_empty());
        assert!(earlier.iter().all(|r| r["final"] == false));
        for r in &reports {
            assert_eq!((&r["live"], &r["records"]), (&false.into(), &0.into()));
            assert_eq!(r["interval_age"]["count"], 0);
        }
        let mut text = Vec::new();
        printer.json = false;
        printer
            .print(&Report::nothing(start, true), &mut text)
            .unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains("not live yet: nothing measured"), "{text}");
    }

    /// What placing the reader returns, a failure included; a report that can't be written; a panic.
    #[test]
    fn placing_the_reader_ends_with_its_result() {
        let none = || (None, None::<Instant>, None);
        let placed = |seek: fn() -> Result<u8>| {
            let (deadline, mut next, interval) = none();
            place(seek, deadline, &mut next, interval, |_| Ok(()))
        };
        assert!(matches!(placed(|| Ok(7)), Ok(Some(7))));
        let failure = placed(|| {
            Err(Failure {
                code: 2,
                message: String::new(),
            })
        });
        assert!(matches!(failure, Err(Failure { code: 2, .. })));
        assert!(placed(|| panic!("expected by the test")).is_err());
        let (_hold, held) = mpsc::channel::<()>();
        let mut due = Some(Instant::now());
        let unwritten = place(
            move || {
                let _ = held.recv();
                Ok(())
            },
            None,
            &mut due,
            Some(Duration::from_secs(1)),
            |_| {
                Err(Failure {
                    code: 1,
                    message: String::new(),
                })
            },
        );
        assert!(matches!(unwritten, Err(Failure { code: 1, .. })));
    }

    /// An idle journal is live, and the records found at the start are not read.
    #[test]
    fn starts_at_the_end_and_live() {
        let dir = journal(5, b"x");
        let set = |args: &mut AgeArgs| args.duration = Some(Duration::from_millis(500));
        let json: serde_json::Value =
            serde_json::from_str(&run_args(dir.path(), true, set)).unwrap();
        assert_eq!((&json["live"], &json["records"]), (&true.into(), &0.into()));
        assert_eq!(json["before_live"], 0);
        let text = run_args(dir.path(), false, set);
        assert!(!text.contains("not live"), "{text}");
    }

    /// The clock is read only now and then while there are records to read, but it is read.
    #[test]
    fn the_clock_is_checked_by_records_and_by_bytes() {
        let due_after = |payload_len| {
            let mut unchecked = Unchecked::default();
            (1..=2000).find(|_| unchecked.add(payload_len))
        };
        assert_eq!(due_after(0), Some(1024));
        assert_eq!(due_after(1023), Some(1024));
        assert_eq!(due_after(1 << 18), Some(4));
        assert_eq!(due_after(usize::MAX), Some(1));
        // From the journal's start and not live, as after a discontinuity.
        let dir = journal(10_000, b"x");
        // Due at every check, an interval report shows where in the scan the checks are.
        let (mut follower, reports) = follower(dir.path());
        follower.interval = Some(Duration::from_nanos(1));
        follower.next_report = Some(Instant::now());
        follower.deadline = Instant::now().checked_add(Duration::from_secs(1));
        follower.follow().unwrap();
        assert!(follower.live, "caught up");
        assert_eq!(follower.age.view().snapshot().before_live, 10_000);
        let scanned: Vec<_> = reports
            .try_iter()
            .map(|r| (r.live, r.ages.before_live))
            .collect();
        assert_eq!(
            scanned,
            [(false, 0), (false, 1024), (false, 2048), (false, 3072)]
        );
    }

    fn follower(dir: &Path) -> (Follower, mpsc::Receiver<Report>) {
        let (sender, reports) = mpsc::sync_channel(PENDING_REPORTS);
        let follower = Follower {
            reader: Reader::open(dir, Wait::Poll.reader_config()).unwrap(),
            age: RecordAge::new(),
            live: false,
            records: 0,
            seqs: None,
            discontinuities: 0,
            start: Instant::now(),
            deadline: None,
            count: None,
            next_report: None,
            interval: None,
            reports: sender,
        };
        (follower, reports)
    }

    /// A standard output that fails ends the command with that error, whatever the reader is doing.
    #[test]
    fn output_failure_ends_the_command() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let _writer = tickertape::Writer::open(
            dir.path(),
            tickertape::WriterConfig::default().segment_size(65536),
        )
        .unwrap();
        let failure = run(
            AgeArgs {
                dir: dir.path().into(),
                duration: None,
                count: None,
                interval: Some(Duration::from_millis(10)),
                durable_only: false,
                wait: Wait::Poll,
                json: true,
            },
            &mut Broken,
        )
        .unwrap_err();
        assert_eq!(failure.code, 1);
    }
}
