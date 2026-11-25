//! `tickertape age` from the outside (ADR 0047).

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::{Child, ChildStdout, Command as ProcessCommand, Output, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;
use tickertape::clock::realtime_ns;
use tickertape::retention::{self, Action, Policy};
use tickertape::{NewRecord, Writer, WriterConfig};

/// How long a test waits for the command before it fails.
const PATIENCE: Duration = Duration::from_secs(60);

fn command() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("tickertape"))
}

fn open(dir: &Path) -> Writer {
    Writer::open(dir, WriterConfig::default().segment_size(65536)).unwrap()
}

/// A journal with `records` records of 20,000 bytes: three fill a segment.
fn journal(records: u64) -> (TempDir, Writer) {
    let dir = TempDir::new().unwrap();
    let mut writer = open(dir.path());
    for _ in 0..records {
        append(&mut writer, realtime_ns());
    }
    (dir, writer)
}

fn append(writer: &mut Writer, local_ts_ns: u64) -> u64 {
    writer
        .append(&NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns,
            payload: &[7; 20_000],
        })
        .unwrap()
}

/// A running `tickertape age --json`.
struct Age {
    child: Child,
    out: BufReader<ChildStdout>,
    reports: Vec<Value>,
}

impl Age {
    fn start(dir: &Path, args: &[&str]) -> Age {
        let mut child = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("tickertape"))
            .arg("age")
            .arg(dir)
            .arg("--json")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Age {
            child,
            out,
            reports: Vec::new(),
        }
    }

    /// The next report; `None` at the end of the output.
    fn next(&mut self) -> Option<Value> {
        let mut line = String::new();
        if self.out.read_line(&mut line).unwrap() == 0 {
            return None;
        }
        assert!(line.ends_with('\n'), "{line}");
        let report: Value = serde_json::from_str(&line).unwrap();
        check(&report);
        self.reports.push(report.clone());
        Some(report)
    }

    /// Reads reports (`--interval`) until one satisfies `done`, calling `act` before each.
    fn until(&mut self, mut act: impl FnMut(), done: impl Fn(&Value) -> bool) -> Value {
        let start = Instant::now();
        loop {
            act();
            let report = self.next().expect("the command ended");
            if done(&report) {
                return report;
            }
            assert!(start.elapsed() < PATIENCE, "{report}");
        }
    }

    /// Waits for the end: every report, and the rest.
    fn finish(mut self) -> (Vec<Value>, Output) {
        while self.next().is_some() {}
        let output = self.child.wait_with_output().unwrap();
        // Exactly one report is the last one, and it is the last one.
        let finals: Vec<bool> = self
            .reports
            .iter()
            .map(|r| r["final"].as_bool().unwrap())
            .collect();
        assert_eq!(finals.iter().filter(|f| **f).count(), 1, "{finals:?}");
        assert_eq!(finals.last(), Some(&true));
        // Everything but the interval is cumulative.
        for pair in self.reports.windows(2) {
            for key in [
                "records",
                "before_live",
                "after_discontinuity",
                "unset_timestamp",
                "ahead_of_clock",
                "discontinuities",
            ] {
                assert!(u(&pair[0], key) <= u(&pair[1], key), "{key}: {pair:?}");
            }
            assert!(u(&pair[0]["age"], "count") <= u(&pair[1]["age"], "count"));
            assert!(pair[0]["elapsed_s"].as_f64() < pair[1]["elapsed_s"].as_f64());
        }
        (self.reports, output)
    }
}

/// Runs to the end while `act` is called in a loop.
fn run_with(mut age: Age, mut act: impl FnMut()) -> (Vec<Value>, Output) {
    let start = Instant::now();
    while age.child.try_wait().unwrap().is_none() {
        act();
        std::thread::sleep(Duration::from_millis(2));
        if start.elapsed() > PATIENCE {
            let _ = age.child.kill();
            panic!("the command did not end");
        }
    }
    age.finish()
}

fn u(v: &Value, key: &str) -> u64 {
    v[key]
        .as_u64()
        .unwrap_or_else(|| panic!("{key} is not a u64 in {v}"))
}

fn keys(v: &Value) -> BTreeSet<&str> {
    v.as_object().unwrap().keys().map(String::as_str).collect()
}

/// The schema in `docs/cli.md` and what holds in every report.
fn check(r: &Value) {
    assert_eq!(
        keys(r),
        BTreeSet::from([
            "schema_version",
            "journal",
            "sampled_unix_ns",
            "elapsed_s",
            "final",
            "live",
            "durable_only",
            "wait",
            "records",
            "first_seq",
            "last_seq",
            "before_live",
            "after_discontinuity",
            "unset_timestamp",
            "ahead_of_clock",
            "discontinuities",
            "age",
            "interval_age",
        ])
    );
    assert_eq!(u(r, "schema_version"), 1);
    assert!(r["journal"].as_str().unwrap().starts_with('/'));
    assert!(u(r, "sampled_unix_ns") > 1_577_836_800_000_000_000);
    assert!(r["elapsed_s"].as_f64().unwrap() >= 0.0);
    assert!(r["durable_only"].is_boolean() && r["live"].is_boolean());
    if !r["live"].as_bool().unwrap() && u(r, "discontinuities") == 0 {
        assert_eq!(u(r, "records"), 0, "{r}");
    }
    assert!(["poll", "spin", "block"].contains(&r["wait"].as_str().unwrap()));
    assert_eq!(
        u(r, "records"),
        u(&r["age"], "count") + u(r, "unset_timestamp") + u(r, "ahead_of_clock")
    );
    if u(r, "records") == 0 {
        assert!(r["first_seq"].is_null() && r["last_seq"].is_null());
    } else {
        assert!(u(r, "first_seq") <= u(r, "last_seq"));
    }
    check_hist(&r["age"]);
    if !r["interval_age"].is_null() {
        check_hist(&r["interval_age"]);
        assert!(u(&r["interval_age"], "count") <= u(&r["age"], "count"));
        assert!(r["interval_age"]["max_ns"].as_u64() <= r["age"]["max_ns"].as_u64());
    }
}

fn check_hist(h: &Value) {
    let values = [
        "min_ns",
        "p50_ns",
        "p90_ns",
        "p99_ns",
        "p99_9_ns",
        "p99_99_ns",
        "max_ns",
    ];
    let mut all = BTreeSet::from(values);
    all.extend(["count", "mean_ns"]);
    assert_eq!(keys(h), all);
    if u(h, "count") == 0 {
        assert!(values.iter().all(|k| h[k].is_null()) && h["mean_ns"].is_null());
        return;
    }
    let ordered: Vec<u64> = values.iter().map(|k| u(h, k)).collect();
    assert!(ordered.windows(2).all(|w| w[0] <= w[1]), "{h}");
    assert!(h["mean_ns"].is_null() || h["mean_ns"].as_f64().unwrap() >= 0.0);
}

fn live(r: &Value) -> bool {
    r["live"].as_bool().unwrap()
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn help_and_usage_errors() {
    let mut help = command().args(["age", "--help"]).assert().success();
    for part in [
        "--duration",
        "--count",
        "--interval",
        "--durable-only",
        "--wait",
        "--json",
        "\"schema_version\":1",
        "THIS COMMAND'S READER",
        "wake-up system call",
        "biased upwards",
        "Exit codes: 0 success; 1 usage",
    ] {
        help = help.stdout(predicate::str::contains(part));
    }
    for args in [
        vec!["age"],
        vec!["age", "x", "--duration", "0s"],
        vec!["age", "x", "--duration", "1d"],
        vec!["age", "x", "--interval", "5"],
        vec!["age", "x", "--count", "-1"],
        vec!["age", "x", "--wait", "sleep"],
        vec!["age", "x", "--format", "json"],
    ] {
        command().args(&args).assert().code(1);
    }
    // A missing journal measured nothing: no report.
    let dir = TempDir::new().unwrap();
    command()
        .args(["age", "--json", "--duration", "1s"])
        .arg(dir.path().join("absent"))
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty());
}

#[test]
fn ages_of_a_live_writer_with_every_wait() {
    for wait in ["poll", "spin", "block"] {
        let (dir, mut writer) = journal(5);
        let age = Age::start(dir.path(), &["--count", "20", "--wait", wait]);
        let (reports, output) = run_with(age, || {
            append(&mut writer, realtime_ns());
        });
        assert_eq!(code(&output), 0, "{output:?}");
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert_eq!(r["wait"], wait);
        assert_eq!(r["durable_only"], false);
        assert_eq!(
            r["journal"],
            dir.path().canonicalize().unwrap().to_str().unwrap()
        );
        assert_eq!((u(r, "records"), u(&r["age"], "count")), (20, 20));
        // The records found at the start are neither read nor measured; the rest is without a gap.
        assert_eq!(u(r, "before_live"), 0, "{r}");
        assert!(u(r, "first_seq") >= 5, "{r}");
        assert_eq!(u(r, "last_seq") - u(r, "first_seq"), 19);
        assert!(r["interval_age"].is_null());
        assert!(!r["age"]["mean_ns"].is_null());
        assert!(u(&r["age"], "max_ns") < PATIENCE.as_nanos() as u64);
        assert_eq!(u(r, "discontinuities"), 0);
    }
}

/// Records without a timestamp and with one ahead of the clock are counted apart, never as an age (of 0).
#[test]
fn unset_and_future_timestamps() {
    let (dir, mut writer) = journal(0);
    let mut age = Age::start(
        dir.path(),
        &["--interval", "20ms", "--duration", "60s", "--count", "30"],
    );
    let mut n = 0u64;
    age.until(
        || {
            for _ in 0..3 {
                let ts = [0, realtime_ns() + 3_600_000_000_000, realtime_ns()][n as usize % 3];
                append(&mut writer, ts);
                n += 1;
            }
        },
        |r| u(r, "records") >= 30,
    );
    let (reports, output) = age.finish();
    assert_eq!(code(&output), 0);
    let r = reports.last().unwrap();
    assert_eq!(u(r, "records"), 30);
    for key in ["unset_timestamp", "ahead_of_clock"] {
        assert!((9..=11).contains(&u(r, key)), "{key}: {r}");
    }
    assert!((9..=11).contains(&u(&r["age"], "count")));
    // In text, the bias is said.
    let (dir, mut writer) = journal(0);
    let child = ProcessCommand::new(assert_cmd::cargo::cargo_bin!("tickertape"))
        .arg("age")
        .arg(dir.path())
        .args(["--count", "1"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = Some(child);
    let start = Instant::now();
    while child.as_mut().unwrap().try_wait().unwrap().is_none() {
        append(&mut writer, u64::MAX);
        std::thread::sleep(Duration::from_millis(2));
        assert!(start.elapsed() < PATIENCE);
    }
    let output = child.take().unwrap().wait_with_output().unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("ahead_of_clock=1 "), "{text}");
    assert!(text.contains("biased"), "{text}");
    assert!(text.contains("age         count=0 min=- "), "{text}");
    assert!(text.contains("  final"), "{text}");
}

/// A tape to which nothing is appended: no record counts, whatever its timestamps are. Also an empty journal, and
/// `--count 0`.
#[test]
fn a_tape_without_a_writer_counts_nothing() {
    let (dir, mut writer) = journal(7);
    writer.checkpoint().unwrap();
    drop(writer);
    let copy = TempDir::new().unwrap();
    for entry in fs::read_dir(dir.path()).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            fs::copy(&path, copy.path().join(path.file_name().unwrap())).unwrap();
        }
    }
    for args in [
        vec!["--duration", "200ms"],
        vec!["--duration", "200ms", "--durable-only"],
        vec!["--duration", "200ms", "--wait", "block"],
    ] {
        let (reports, output) = Age::start(copy.path(), &args).finish();
        assert_eq!(code(&output), 0);
        assert_eq!(reports.len(), 1);
        let r = &reports[0];
        assert_eq!((u(r, "records"), u(&r["age"], "count")), (0, 0));
        assert_eq!(u(r, "before_live"), 0);
        assert!(live(r), "an idle journal is live: {r}");
        assert!(r["elapsed_s"].as_f64().unwrap() >= 0.2);
        assert_eq!(r["durable_only"], args.contains(&"--durable-only"));
    }
    let (empty, writer) = journal(0);
    let (reports, output) = Age::start(empty.path(), &["--duration", "100ms"]).finish();
    assert_eq!((code(&output), live(&reports[0])), (0, true));
    drop(writer);
    let (reports, output) = Age::start(copy.path(), &["--count", "0"]).finish();
    assert_eq!((code(&output), u(&reports[0], "records")), (0, 0));
    assert!(live(&reports[0]));
}

/// Where the reader can't be placed at the end, `age` fails as `tail --from-end` does, and has nothing to report. Damage
/// in an older segment's records does not matter to either: they are not read.
#[test]
fn starts_where_tail_from_end_does() {
    let both = |dir: &Path, options: &[&str]| {
        let tail = command()
            .args(["tail", "--from-end", "--no-follow"])
            .args(options)
            .arg(dir)
            .output()
            .unwrap();
        let age = command()
            .args(["age", "--json", "--duration", "100ms"])
            .args(options)
            .arg(dir)
            .output()
            .unwrap();
        assert_eq!(code(&tail), code(&age), "{tail:?} {age:?}");
        if code(&age) == 0 {
            let report: Value = serde_json::from_slice(&age.stdout).unwrap();
            check(&report);
            assert!(live(&report) && u(&report, "records") == 0, "{report}");
        } else {
            assert!(age.stdout.is_empty(), "{age:?}");
            assert_eq!(stderr(&age), stderr(&tail));
        }
        code(&age)
    };
    let damage = |dir: &Path, base: u64| {
        fs::OpenOptions::new()
            .write(true)
            .open(dir.join(tickertape::format::segment_file_name(base)))
            .unwrap()
            .write_all_at(&[0xff; 8], 64 + 60)
            .unwrap();
    };
    let empty = TempDir::new().unwrap();
    assert_eq!(both(empty.path(), &[]), 1);
    assert_eq!(both(&empty.path().join("absent"), &[]), 1);
    let (dir, writer) = journal(0);
    assert_eq!(both(dir.path(), &[]), 0);
    assert_eq!(both(dir.path(), &["--durable-only"]), 0);
    drop(writer);
    // Two records in the newest segment, the fourth and fifth of the journal.
    let (dir, writer) = journal(5);
    drop(writer);
    damage(dir.path(), 0);
    assert_eq!(both(dir.path(), &[]), 0);
    damage(dir.path(), 3);
    assert_eq!(both(dir.path(), &[]), 2);
}

/// Durable-only without a checkpoint: nothing is covered, so the reader is at the start, and the records the first
/// checkpoint covers are new to it, however long ago they were appended. That is their age when they became readable.
#[test]
fn durable_only_before_the_first_checkpoint() {
    let (dir, mut writer) = journal(4);
    let mut age = Age::start(
        dir.path(),
        &["--durable-only", "--interval", "20ms", "--count", "4"],
    );
    let r = age.until(|| (), live);
    assert_eq!(u(&r, "records"), 0);
    std::thread::sleep(Duration::from_millis(100));
    writer.checkpoint().unwrap();
    let (reports, output) = age.finish();
    assert_eq!(code(&output), 0);
    let r = reports.last().unwrap();
    assert_eq!((u(r, "first_seq"), u(r, "last_seq")), (0, 3));
    assert!(u(&r["age"], "min_ns") >= 100_000_000, "{r}");
}

/// Paced by the reports rather than by time, so that it does not depend on how fast the machine is.
#[test]
fn interval_reports() {
    let (dir, mut writer) = journal(2);
    let mut age = Age::start(dir.path(), &["--interval", "20ms", "--count", "40"]);
    age.until(|| (), live);
    for _ in 0..3 {
        append(&mut writer, realtime_ns());
        age.next().unwrap();
    }
    for _ in 0..40 {
        append(&mut writer, realtime_ns());
    }
    let (reports, output) = age.finish();
    assert_eq!(code(&output), 0);
    assert!(reports.len() >= 5, "{}", reports.len());
    let last = reports.last().unwrap();
    assert_eq!((u(last, "records"), u(last, "first_seq")), (40, 2));
    // The intervals add up to the whole.
    let sum: u64 = reports.iter().map(|r| u(&r["interval_age"], "count")).sum();
    assert_eq!(sum, u(&last["age"], "count"));
}

/// With `--durable-only` the age includes the wait for the checkpoint.
#[test]
fn durable_only_ages_span_the_wait_for_the_checkpoint() {
    let (dir, mut writer) = journal(1);
    writer.checkpoint().unwrap();
    let mut age = Age::start(
        dir.path(),
        &["--durable-only", "--interval", "20ms", "--count", "3"],
    );
    age.until(|| (), live);
    for _ in 0..3 {
        append(&mut writer, realtime_ns());
    }
    std::thread::sleep(Duration::from_millis(250));
    writer.checkpoint().unwrap();
    let (reports, output) = age.finish();
    assert_eq!(code(&output), 0);
    let r = reports.last().unwrap();
    assert_eq!((u(r, "records"), u(r, "first_seq")), (3, 1));
    assert_eq!(r["durable_only"], true);
    assert!(u(&r["age"], "min_ns") >= 250_000_000, "{r}");
}

/// Corruption met while following: the report of what was measured, then exit 2.
#[test]
fn corruption_while_following() {
    let (dir, mut writer) = journal(0);
    let mut age = Age::start(dir.path(), &["--interval", "20ms", "--duration", "60s"]);
    age.until(
        || {
            append(&mut writer, realtime_ns());
        },
        |r| u(r, "records") >= 1,
    );
    let head = tickertape::status::status(dir.path()).unwrap().head;
    drop(writer);
    let segment = dir.path().join(tickertape::format::segment_file_name(
        head.position.segment_base_seq,
    ));
    fs::OpenOptions::new()
        .write(true)
        .open(segment)
        .unwrap()
        .write_all_at(&7u32.to_le_bytes(), head.position.offset)
        .unwrap();
    let (reports, output) = age.finish();
    assert_eq!(code(&output), 2, "{output:?}");
    assert!(u(reports.last().unwrap(), "records") >= 1);
    assert!(stderr(&output).contains("tickertape: "), "{output:?}");
}

/// History discarded by a recovery while following: the command resumes as `tail` does, and counts only what is
/// appended after it has caught up again.
#[test]
fn discarded_history_while_following() {
    let (dir, mut writer) = journal(0);
    let mut age = Age::start(dir.path(), &["--interval", "20ms", "--duration", "60s"]);
    let before = age.until(
        || {
            append(&mut writer, realtime_ns());
        },
        |r| u(r, "records") >= 1,
    );
    let next = writer.next_seq();
    age.until(|| (), |r| r["last_seq"].as_u64() == Some(next - 1));
    drop(writer);
    // Damage the first record of the newest segment: recovery removes it and what follows.
    let head = tickertape::status::status(dir.path()).unwrap().head;
    let base = head.position.segment_base_seq;
    fs::OpenOptions::new()
        .write(true)
        .open(dir.path().join(tickertape::format::segment_file_name(base)))
        .unwrap()
        .write_all_at(&[0xff; 8], 64 + 60)
        .unwrap();
    let mut writer = open(dir.path());
    assert!(writer.next_seq() <= next);
    while writer.next_seq() < next {
        append(&mut writer, realtime_ns());
    }
    let after = age.until(
        || {
            append(&mut writer, realtime_ns());
        },
        |r| u(r, "discontinuities") == 1 && u(r, "records") > u(&before, "records") + 3,
    );
    assert!(u(&after, "last_seq") >= next);
    let _ = age.child.kill();
    let output = age.child.wait_with_output().unwrap();
    assert!(
        stderr(&output).contains("warning: history discarded at sequence"),
        "{output:?}"
    );
}

/// Retention removes records a held-up durable-only `age` has not read: it resumes at the oldest one left, warns, and
/// counts what it finds there as `after_discontinuity`.
#[test]
fn records_removed_while_following() {
    let (dir, mut writer) = journal(1);
    writer.checkpoint().unwrap();
    let mut age = Age::start(
        dir.path(),
        &["--durable-only", "--interval", "20ms", "--duration", "60s"],
    );
    age.until(|| (), live);
    let signal = |signal: &str| {
        let status = ProcessCommand::new("kill")
            .arg(signal)
            .arg(age.child.id().to_string())
            .status()
            .unwrap();
        assert!(status.success());
    };
    signal("-STOP");
    for _ in 0..12 {
        append(&mut writer, realtime_ns());
    }
    writer.checkpoint().unwrap();
    let plan = retention::plan(dir.path(), &Policy::default().max_segments(2)).unwrap();
    retention::apply(dir.path(), &plan, &Action::Delete).unwrap();
    let oldest = tickertape::inspect::list_segments(dir.path()).unwrap()[0].base_seq;
    assert!(oldest > 1);
    signal("-CONT");
    let r = age.until(|| (), |r| u(r, "discontinuities") == 1);
    let r = if u(&r, "after_discontinuity") == 13 - oldest {
        r
    } else {
        age.until(|| (), |r| u(r, "after_discontinuity") == 13 - oldest)
    };
    assert_eq!(u(&r, "records"), 0, "{r}");
    // Live again: what is appended now counts.
    age.until(|| (), live);
    append(&mut writer, realtime_ns());
    writer.checkpoint().unwrap();
    let r = age.until(|| (), |r| u(r, "records") == 1);
    assert_eq!(u(&r, "first_seq"), 13);
    let _ = age.child.kill();
    let output = age.child.wait_with_output().unwrap();
    assert!(
        stderr(&output).contains(&format!(
            "warning: records 1 to {} were removed before age read them; continuing at sequence {oldest}",
            oldest - 1
        )),
        "{output:?}"
    );
}

/// `poll` and `spin` never register as a waiter in the writer's wake-up region, so they never add a wake-up system call
/// to its appends; `block` does (so the check can tell the difference). The count of waiters is at byte 64 of the region
/// (ADR 0017), which on Linux is a memfd of the writer, here this process: the journal's `notify` record holds its
/// descriptor at byte 28 (ADR 0021). On macOS it is a shared-memory object, which can't be read without mapping it; there
/// `src/age.rs` tests the reader's settings, which decide it on every platform.
#[test]
#[cfg(target_os = "linux")]
fn only_block_registers_as_a_waiter() {
    for (wait, registers) in [("block", true), ("poll", false), ("spin", false)] {
        let (dir, mut writer) = journal(3);
        let record = fs::read(dir.path().join("notify")).unwrap();
        let fd = i32::from_le_bytes(record[28..32].try_into().unwrap());
        let region = fs::File::open(format!("/proc/self/fd/{fd}")).unwrap();
        let mut age = Age::start(
            dir.path(),
            &["--wait", wait, "--interval", "20ms", "--duration", "1s"],
        );
        age.until(|| (), live);
        let mut seen = 0;
        let start = Instant::now();
        let mut samples = 0;
        while start.elapsed() < Duration::from_millis(400) {
            let mut waiters = [0; 4];
            region.read_exact_at(&mut waiters, 64).unwrap();
            seen = seen.max(u32::from_le_bytes(waiters));
            samples += 1;
            if samples % 100 == 0 {
                append(&mut writer, realtime_ns());
            }
            std::thread::sleep(Duration::from_micros(100));
        }
        assert_eq!(seen > 0, registers, "{wait}: {seen} waiters");
        let (reports, output) = age.finish();
        assert_eq!(code(&output), 0);
        assert!(u(reports.last().unwrap(), "records") > 0);
    }
}

/// The text output is for people; every number of the JSON is in it.
#[test]
fn text_output() {
    let (dir, writer) = journal(4);
    command()
        .args(["age", "--duration", "250ms", "--interval", "100ms"])
        .arg(dir.path())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("journal     /")
                .and(predicate::str::contains("wait=poll"))
                .and(predicate::str::contains("before_live=0"))
                .and(predicate::str::contains("interval    count=0"))
                .and(predicate::str::contains("  final")),
        );
    drop(writer);
}
