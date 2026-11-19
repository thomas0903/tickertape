//! `tickertape status`: operational metrics as text, JSON or Prometheus text format, and a health check (ADR 0032).

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, ValueEnum};
use serde::Serialize;
use tickertape::status::{
    BeaconDirState, Breach, HeadEnd, ReaderStatus, Sampler, Status, Thresholds,
};

use crate::{Failure, Result, archive, output, report};

/// Exit code for a breached health threshold.
const UNHEALTHY: u8 = 3;
/// Exit code when the sample found the journal corrupt (as `verify`).
const CORRUPT: u8 = 2;
/// Version of the JSON object; bumped when a field is removed or changes meaning (adding fields doesn't bump it).
const SCHEMA_VERSION: u32 = 1;

pub(crate) const HELP: &str = "Samples the journal without taking the writer lock or writing to it: the writer's head, the checkpoint and what a power loss would remove now (exposure), space left on the volume, the writer process, and readers that publish a beacon (tickertape::status::ReaderBeacon) in <DIR>/readers or --readers-dir. Values a sample can't establish are null; estimates are named estimated_*. head.verified and exposure.verified say whether the sample validated the records between the checkpoint and the head (it reads only the newest segment). Cost: a scan of the newest segment from the checkpoint (or its start; with --watch only what was appended since the previous sample), one stat per segment, statvfs, and a few small reads per beacon.\n\nJSON schema (schema_version 1; fields may be added): see docs/cli.md. With --watch, one object per line; a sample that fails is a line {\"schema_version\":1,\"journal\":string,\"sampled_unix_ns\":u64,\"error\":{\"code\":string,\"message\":string}} and the watch continues.\n\nPrometheus: text exposition format 0.0.4, gauges named tickertape_*, labelled journal=\"<canonical DIR>\" (and reader=\"<name>\"); tickertape_up is 0 for a failed sample and tickertape_status_timestamp_seconds always tells when the file was written; see docs/cli.md. --watch with --format prometheus needs --output.\n\nExit codes: 0 healthy (or help, or --watch finished); 1 usage, I/O or missing journal; 2 the sample found the journal corrupt (the head at invalid bytes, a checkpoint marker the writer would refuse, a damaged newest segment header) or an unsupported format, which takes precedence over 3; 3 a health threshold is breached (not with --watch, which reports health in each sample).";

#[derive(Debug, Args)]
pub(crate) struct StatusArgs {
    /// Journal directory (read-only; no writer lock)
    dir: PathBuf,
    /// Output format
    #[arg(long, value_enum, default_value = "text")]
    format: StatusFormat,
    /// Shorthand for --format json
    #[arg(long, conflicts_with = "format")]
    json: bool,
    /// Read reader beacons from this directory instead of <DIR>/readers
    #[arg(long, value_name = "DIR")]
    readers_dir: Option<PathBuf>,
    /// Sample every INTERVAL (integer followed by ms, s, m or h) until interrupted
    #[arg(long, value_name = "INTERVAL", value_parser = parse_interval)]
    watch: Option<Duration>,
    /// With --watch, stop after N samples
    #[arg(long, value_name = "N", requires = "watch", value_parser = clap::value_parser!(u64).range(1..))]
    samples: Option<u64>,
    /// Write each sample to FILE, replacing it atomically (for a node_exporter textfile collector); outside the journal
    #[arg(long, value_name = "FILE")]
    output: Option<PathBuf>,
    /// Unhealthy if records are uncovered and the checkpoint's estimated age is above DUR (integer followed by d, h, m or s) or unknown, or there is no checkpoint
    #[arg(long, value_name = "DUR", value_parser = archive::parse_duration)]
    max_checkpoint_age: Option<Duration>,
    /// Unhealthy if fewer than B bytes are available on the volume (optional K/M/G/T suffix)
    #[arg(long, value_name = "B", value_parser = archive::parse_bytes)]
    min_headroom: Option<u64>,
    /// Unhealthy if fewer than N more segments are estimated to fit on the volume
    #[arg(long, value_name = "N")]
    min_headroom_segments: Option<u64>,
    /// Unhealthy if a live reader beacon lags by more than N records
    #[arg(long, value_name = "N")]
    max_reader_lag: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum StatusFormat {
    Text,
    Json,
    Prometheus,
}

fn parse_interval(value: &str) -> std::result::Result<Duration, String> {
    let duration = match value.strip_suffix("ms") {
        Some(ms) if !ms.is_empty() && ms.bytes().all(|b| b.is_ascii_digit()) => ms
            .parse::<u64>()
            .map(Duration::from_millis)
            .map_err(|_| "value exceeds u64 range".to_string())?,
        Some(_) => return Err("expected an integer followed by ms, s, m or h".into()),
        None if value.ends_with('d') => {
            return Err("expected an integer followed by ms, s, m or h".into());
        }
        None => archive::parse_duration(value)?,
    };
    if duration.is_zero() {
        return Err("interval must be greater than zero".into());
    }
    Ok(duration)
}

pub(crate) fn run(args: StatusArgs, out: &mut dyn Write) -> Result<()> {
    let format = if args.json {
        StatusFormat::Json
    } else {
        args.format
    };
    if args.watch.is_some() && format == StatusFormat::Prometheus && args.output.is_none() {
        return Err(Failure::usage(
            "--watch with --format prometheus needs --output: repeated expositions in one stream are not valid",
        ));
    }
    let mut thresholds = Thresholds::default();
    thresholds.max_checkpoint_age = args.max_checkpoint_age;
    thresholds.min_headroom_bytes = args.min_headroom;
    thresholds.min_headroom_segments = args.min_headroom_segments;
    thresholds.max_reader_lag = args.max_reader_lag;
    // The canonical path, so that `./j` and `/abs/j` are the same series and the same JSON value.
    let journal = fs::canonicalize(&args.dir)
        .map_err(|e| output::io_error("resolve journal directory", e))?;
    let output = args
        .output
        .as_deref()
        .map(|path| destination(&journal, path))
        .transpose()?;
    let mut sampler = Sampler::new(&journal);
    if let Some(readers) = &args.readers_dir {
        sampler = sampler.readers_dir(readers);
    }
    let mut taken = 0u64;
    loop {
        let sampled = sampler.sample();
        let mut text = String::new();
        let outcome = match &sampled {
            Ok(sample) => {
                let breaches = sample.breaches(&thresholds);
                match format {
                    StatusFormat::Text => render_text(&mut text, &journal, sample, &breaches),
                    StatusFormat::Json => {
                        json_line(&mut text, &JsonStatus::new(&journal, sample, &breaches))?
                    }
                    StatusFormat::Prometheus => {
                        render_prometheus(&mut text, &journal, Some((sample, &breaches)), None);
                    }
                }
                Ok(breaches)
            }
            Err(error) => {
                let now = tickertape::clock::realtime_ns();
                match format {
                    StatusFormat::Text => {
                        let _ = writeln!(text, "journal     {}", journal.display());
                        let _ = writeln!(text, "error       {error}");
                    }
                    StatusFormat::Json => json_line(
                        &mut text,
                        &JsonError {
                            schema_version: SCHEMA_VERSION,
                            journal: journal.to_string_lossy().into_owned(),
                            sampled_unix_ns: now,
                            error: JsonErrorBody {
                                code: error_code(error),
                                message: error.to_string(),
                            },
                        },
                    )?,
                    StatusFormat::Prometheus => {
                        render_prometheus(&mut text, &journal, None, Some(now));
                    }
                }
                Err(())
            }
        };
        // A one-shot sample that failed prints nothing on stdout (the error goes to stderr), but still replaces an
        // --output file, so that a collector sees `tickertape_up 0` rather than a stale healthy sample.
        let print = args.watch.is_some() || outcome.is_ok() || output.is_some();
        if print {
            match &output {
                Some(path) => replace_file(path, text.as_bytes())?,
                None => {
                    if taken > 0 && format == StatusFormat::Text {
                        writeln!(out).map_err(|e| output::io_error("write output", e))?;
                    }
                    out.write_all(text.as_bytes())
                        .and_then(|()| out.flush())
                        .map_err(|e| output::io_error("write output", e))?;
                }
            }
        }
        taken += 1;
        let Some(interval) = args.watch else {
            let breaches = match (sampled, outcome) {
                (Err(error), _) => return Err(error.into()),
                (Ok(_), Ok(breaches)) => breaches,
                (Ok(_), Err(())) => unreachable!("a sample's outcome follows the sample"),
            };
            if breaches.is_empty() {
                return Ok(());
            }
            let codes: Vec<String> = breaches.iter().map(describe).collect();
            let code = if breaches.iter().any(Breach::is_corruption) {
                CORRUPT
            } else {
                UNHEALTHY
            };
            return Err(Failure {
                code,
                message: format!("unhealthy: {}", codes.join(", ")),
            });
        };
        if args.samples.is_some_and(|n| taken >= n) {
            return Ok(());
        }
        std::thread::sleep(interval);
    }
}

fn json_line(text: &mut String, value: &impl Serialize) -> Result<()> {
    let json = serde_json::to_string(value).map_err(|e| output::io_error("encode JSON", e))?;
    text.push_str(&json);
    text.push('\n');
    Ok(())
}

fn error_code(error: &tickertape::Error) -> &'static str {
    match error {
        tickertape::Error::NotFound { .. } => "not_found",
        tickertape::Error::Corrupt { .. } => "corrupt",
        tickertape::Error::UnsupportedVersion { .. } => "unsupported_version",
        _ => "io",
    }
}

fn describe(breach: &Breach) -> String {
    match breach {
        Breach::ReaderLag { name } => format!("reader_lag({name})"),
        other => other.code().to_string(),
    }
}

/// Resolves `path` for `--output`: its directory must exist and be outside the journal.
fn destination(journal: &Path, path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| Failure::usage("--output must name a file"))?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let parent =
        fs::canonicalize(parent).map_err(|e| output::io_error("resolve output directory", e))?;
    if parent.starts_with(journal) {
        return Err(Failure::usage(
            "status output must be outside the journal directory",
        ));
    }
    Ok(parent.join(name))
}

/// The `n`th candidate temporary name for replacing `path`, in the same directory.
fn temp_path(path: &Path, pid: u32, n: u32) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!(".{pid}.{n}.tmp"));
    path.with_file_name(name)
}

/// Writes `bytes` to a new, uniquely named temporary file next to `path` (created exclusively, so it never follows a
/// symlink or opens an existing file) and renames it over `path`, so readers never see a partial file. The temporary
/// file is removed on every error.
fn replace_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let pid = std::process::id();
    let mut n = 0u32;
    let (tmp, mut file) = loop {
        let tmp = temp_path(path, pid, n);
        // `create_new` is O_CREAT | O_EXCL (std adds O_CLOEXEC).
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => break (tmp, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 1000 => n += 1,
            Err(e) => return Err(output::io_error("create temporary output", e)),
        }
    };
    let written = file
        .write_all(bytes)
        .map_err(|e| output::io_error("write output", e))
        .and_then(|()| {
            drop(file);
            fs::rename(&tmp, path).map_err(|e| output::io_error("replace output", e))
        });
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

fn end_name(end: HeadEnd) -> &'static str {
    match end {
        HeadEnd::Open => "open",
        HeadEnd::Sealed => "sealed",
        HeadEnd::Invalid(_) => "invalid",
        _ => "unknown",
    }
}

fn dir_state(state: BeaconDirState) -> (&'static str, Option<String>) {
    match state {
        BeaconDirState::Listed => ("listed", None),
        BeaconDirState::Missing => ("missing", None),
        BeaconDirState::Unusable(kind) => ("unusable", Some(kind.to_string())),
        _ => ("unknown", None),
    }
}

#[derive(Serialize)]
struct JsonError {
    schema_version: u32,
    journal: String,
    sampled_unix_ns: u64,
    error: JsonErrorBody,
}

#[derive(Serialize)]
struct JsonErrorBody {
    code: &'static str,
    message: String,
}

#[derive(Serialize)]
struct JsonStatus {
    schema_version: u32,
    journal: String,
    sampled_unix_ns: u64,
    head: JsonHead,
    segments: JsonSegments,
    checkpoint: Option<JsonCheckpoint>,
    exposure: JsonExposure,
    storage: JsonStorage,
    rates: Option<JsonRates>,
    writer: JsonWriter,
    readers_dir: JsonReadersDir,
    readers: Vec<JsonReader>,
    health: JsonHealth,
}

#[derive(Serialize)]
struct JsonHead {
    next_seq: u64,
    segment_base_seq: u64,
    offset: u64,
    segment_size_bytes: u64,
    end: &'static str,
    invalid_reason: Option<String>,
    last_local_ts_ns: Option<u64>,
    verified: bool,
}

#[derive(Serialize)]
struct JsonSegments {
    count: u64,
    first_seq: u64,
    total_bytes: u64,
    oldest_created_unix_ns: Option<u64>,
    before_checkpoint: u64,
}

#[derive(Serialize)]
struct JsonCheckpoint {
    generation: u64,
    next_seq: u64,
    segment_base_seq: u64,
    offset: u64,
    problem: Option<&'static str>,
    modified_unix_ns: Option<u64>,
    estimated_age_s: Option<f64>,
    age_unknown_reason: Option<&'static str>,
}

#[derive(Serialize)]
struct JsonExposure {
    records: Option<u64>,
    bytes: Option<u64>,
    estimated_age_s: Option<f64>,
    oldest_local_ts_ns: Option<u64>,
    verified: bool,
}

#[derive(Serialize)]
struct JsonStorage {
    available_bytes: u64,
    total_bytes: u64,
    next_segment_size_bytes: u64,
    spare_segment: bool,
    estimated_rolls_left: u64,
    estimated_time_to_full_s: Option<f64>,
}

#[derive(Serialize)]
struct JsonRates {
    interval_s: f64,
    records_per_s: f64,
    bytes_per_s: Option<f64>,
}

#[derive(Serialize)]
struct JsonWriter {
    pid: Option<u32>,
    alive: Option<bool>,
}

#[derive(Serialize)]
struct JsonReadersDir {
    path: String,
    state: &'static str,
    error: Option<String>,
}

#[derive(Serialize)]
struct JsonReader {
    name: String,
    state: &'static str,
    pid: Option<u32>,
    next_seq: Option<u64>,
    durable_only: Option<bool>,
    seek_pending: Option<bool>,
    lag_records: Option<u64>,
    lag_bytes: Option<u64>,
    lag_unknown_reason: Option<&'static str>,
    unread_age_s: Option<f64>,
    oldest_unread_local_ts_ns: Option<u64>,
    updated_unix_ns: Option<u64>,
    update_age_s: Option<f64>,
}

#[derive(Serialize)]
struct JsonHealth {
    ok: bool,
    corrupt: bool,
    breaches: Vec<JsonBreach>,
}

#[derive(Serialize)]
struct JsonBreach {
    code: &'static str,
    reader: Option<String>,
}

impl JsonStatus {
    fn new(journal: &Path, s: &Status, breaches: &[Breach]) -> JsonStatus {
        let (state, error) = dir_state(s.readers_dir_state);
        JsonStatus {
            schema_version: SCHEMA_VERSION,
            journal: journal.to_string_lossy().into_owned(),
            sampled_unix_ns: s.sampled_unix_ns,
            head: JsonHead {
                next_seq: s.head.position.next_seq,
                segment_base_seq: s.head.position.segment_base_seq,
                offset: s.head.position.offset,
                segment_size_bytes: s.head.segment_size,
                end: end_name(s.head.end),
                invalid_reason: match s.head.end {
                    HeadEnd::Invalid(reason) => Some(reason.to_string()),
                    _ => None,
                },
                last_local_ts_ns: s.head.last_local_ts_ns,
                verified: s.head.verified,
            },
            segments: JsonSegments {
                count: s.segments.count,
                first_seq: s.segments.first_seq,
                total_bytes: s.segments.total_bytes,
                oldest_created_unix_ns: s.segments.oldest_created_unix_ns,
                before_checkpoint: s.segments.before_checkpoint,
            },
            checkpoint: s.checkpoint.map(|c| JsonCheckpoint {
                generation: c.marker.generation,
                next_seq: c.marker.next_seq,
                segment_base_seq: c.marker.segment_base_seq,
                offset: c.marker.offset,
                problem: c.problem.map(|p| p.code()),
                modified_unix_ns: c.modified_unix_ns,
                estimated_age_s: c.estimated_age.map(secs),
                age_unknown_reason: c.age_unknown.map(|a| a.code()),
            }),
            exposure: JsonExposure {
                records: s.exposure.records,
                bytes: s.exposure.bytes,
                estimated_age_s: s.exposure.estimated_age.map(secs),
                oldest_local_ts_ns: s.exposure.oldest_local_ts_ns,
                verified: s.exposure.verified,
            },
            storage: JsonStorage {
                available_bytes: s.storage.available_bytes,
                total_bytes: s.storage.total_bytes,
                next_segment_size_bytes: s.storage.next_segment_size,
                spare_segment: s.storage.spare_segment,
                estimated_rolls_left: s.storage.estimated_rolls_left,
                estimated_time_to_full_s: s.storage.estimated_time_to_full.map(secs),
            },
            rates: s.rates.map(|r| JsonRates {
                interval_s: secs(r.interval),
                records_per_s: r.records_per_sec,
                bytes_per_s: r.bytes_per_sec,
            }),
            writer: JsonWriter {
                pid: s.writer.pid,
                alive: s.writer.alive,
            },
            readers_dir: JsonReadersDir {
                path: s.readers_dir.to_string_lossy().into_owned(),
                state,
                error,
            },
            readers: s
                .readers
                .iter()
                .map(|r| JsonReader {
                    name: r.name.clone(),
                    state: r.state.code(),
                    pid: r.report.map(|p| p.pid),
                    next_seq: r.report.map(|p| p.next_seq),
                    durable_only: r.report.map(|p| p.durable_only),
                    seek_pending: r.report.map(|p| p.seek_pending),
                    lag_records: r.lag.map(|l| l.records),
                    lag_bytes: r.lag.and_then(|l| l.bytes),
                    lag_unknown_reason: r.lag_unknown.map(|u| u.code()),
                    unread_age_s: r.unread_age.map(secs),
                    oldest_unread_local_ts_ns: r.oldest_unread_local_ts_ns,
                    updated_unix_ns: r.report.map(|p| p.updated_unix_ns),
                    update_age_s: r.update_age.map(secs),
                })
                .collect(),
            health: JsonHealth {
                ok: breaches.is_empty(),
                corrupt: breaches.iter().any(Breach::is_corruption),
                breaches: breaches
                    .iter()
                    .map(|b| JsonBreach {
                        code: b.code(),
                        reader: match b {
                            Breach::ReaderLag { name } => Some(name.clone()),
                            _ => None,
                        },
                    })
                    .collect(),
            },
        }
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn human_duration(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 60.0 {
        format!("{s:.3} s")
    } else if s < 3600.0 {
        format!("{:.1} min", s / 60.0)
    } else if s < 86_400.0 {
        format!("{:.1} h", s / 3600.0)
    } else {
        format!("{:.1} d", s / 86_400.0)
    }
}

fn opt_ts(ts: Option<u64>) -> String {
    ts.map_or_else(|| "-".to_string(), report::rfc3339)
}

/// Why the exposure's age is unknown, in words.
fn exposure_age_unknown(s: &Status) -> String {
    match &s.checkpoint {
        None => "unknown (no checkpoint)".into(),
        Some(c) if c.problem.is_some() => "unknown (checkpoint marker unusable)".into(),
        Some(c) => match c.age_unknown {
            Some(reason) => format!("unknown ({})", age_unknown_words(reason)),
            None => "unknown".into(),
        },
    }
}

fn age_unknown_words(reason: tickertape::status::AgeUnknown) -> &'static str {
    use tickertape::status::AgeUnknown;
    match reason {
        AgeUnknown::NoModificationTime => "the filesystem reports no modification time",
        AgeUnknown::InFuture => "the marker's modification time is in the future",
        AgeUnknown::BeforeSegment => "the marker's modification time predates its segment",
        AgeUnknown::MarkerResized => "the marker file was resized",
        AgeUnknown::Changing => "the marker kept changing while read",
        _ => "unreliable modification time",
    }
}

// Writing to a String never fails, so the `fmt::Result`s below are ignored.
fn render_text(out: &mut String, journal: &Path, s: &Status, breaches: &[Breach]) {
    let unverified = if s.head.verified {
        ""
    } else {
        " (chain from the checkpoint not verified by this sample)"
    };
    let _ = writeln!(out, "journal     {}", journal.display());
    let end = match s.head.end {
        HeadEnd::Invalid(reason) => format!("invalid: {reason}"),
        other => end_name(other).to_string(),
    };
    let _ = writeln!(
        out,
        "head        next seq {}, segment {} offset {} ({end}), last local ts {}{unverified}",
        s.head.position.next_seq,
        s.head.position.segment_base_seq,
        s.head.position.offset,
        opt_ts(s.head.last_local_ts_ns)
    );
    let _ = writeln!(
        out,
        "segments    {} ({}), first seq {}, oldest created {}, {} before the checkpoint segment",
        s.segments.count,
        human_bytes(s.segments.total_bytes),
        s.segments.first_seq,
        opt_ts(s.segments.oldest_created_unix_ns),
        s.segments.before_checkpoint
    );
    match &s.checkpoint {
        None => {
            let _ = writeln!(out, "checkpoint  none");
        }
        Some(c) => {
            let age = match (c.estimated_age, c.age_unknown) {
                (Some(age), _) => format!("about {} ago", human_duration(age)),
                (None, Some(reason)) => format!("age unknown: {}", age_unknown_words(reason)),
                (None, None) => "age unknown".into(),
            };
            let problem = c.problem.map_or_else(String::new, |p| {
                format!(", UNUSABLE ({}): the writer refuses this journal", p.code())
            });
            let _ = writeln!(
                out,
                "checkpoint  generation {}, next seq {}, marker modified {} ({age}){problem}",
                c.marker.generation,
                c.marker.next_seq,
                opt_ts(c.modified_unix_ns),
            );
        }
    }
    let records = s
        .exposure
        .records
        .map_or_else(|| "unknown".into(), |r| r.to_string());
    let _ = writeln!(
        out,
        "exposure    {records} records, {}, exposed for up to {}{}",
        s.exposure
            .bytes
            .map_or_else(|| "? bytes".into(), human_bytes),
        s.exposure
            .estimated_age
            .map_or_else(|| exposure_age_unknown(s), human_duration),
        if s.exposure.verified {
            ""
        } else {
            " (not verified)"
        }
    );
    let _ = writeln!(
        out,
        "storage     {} available of {}, about {} more segments of {}{}{}",
        human_bytes(s.storage.available_bytes),
        human_bytes(s.storage.total_bytes),
        s.storage.estimated_rolls_left,
        human_bytes(s.storage.next_segment_size),
        if s.storage.spare_segment {
            " (and one prepared)"
        } else {
            ""
        },
        s.storage
            .estimated_time_to_full
            .map_or_else(String::new, |t| format!(
                ", full in about {}",
                human_duration(t)
            ))
    );
    if let Some(r) = &s.rates {
        let _ = writeln!(
            out,
            "rates       {:.1} records/s, {}/s over {}",
            r.records_per_sec,
            r.bytes_per_sec
                .map_or_else(|| "?".into(), |b| human_bytes(b as u64)),
            human_duration(r.interval)
        );
    }
    let _ = writeln!(
        out,
        "writer      {}",
        match (s.writer.pid, s.writer.alive) {
            (None, _) => "unknown".to_string(),
            (Some(pid), Some(true)) => format!("pid {pid} (running)"),
            (Some(pid), Some(false)) => format!("pid {pid} (not running)"),
            (Some(pid), None) => format!("pid {pid}"),
        }
    );
    match (s.readers_dir_state, s.readers.is_empty()) {
        (BeaconDirState::Unusable(kind), _) => {
            let _ = writeln!(
                out,
                "readers     beacon directory {} unusable: {kind}",
                s.readers_dir.display()
            );
        }
        (_, true) => {
            let _ = writeln!(out, "readers     none");
        }
        _ => {}
    }
    for (i, r) in s.readers.iter().enumerate() {
        let label = if i == 0 { "readers" } else { "" };
        let _ = writeln!(
            out,
            "{label:<11} {} {}{}",
            r.name,
            r.state.code(),
            reader_detail(r)
        );
    }
    if breaches.is_empty() {
        let _ = writeln!(out, "health      ok");
    } else {
        let codes: Vec<String> = breaches.iter().map(describe).collect();
        let what = if breaches.iter().any(Breach::is_corruption) {
            "CORRUPT"
        } else {
            "UNHEALTHY"
        };
        let _ = writeln!(out, "health      {what}: {}", codes.join(", "));
    }
}

fn reader_detail(r: &ReaderStatus) -> String {
    let Some(report) = r.report else {
        return String::new();
    };
    let lag = match (r.lag, r.lag_unknown) {
        (Some(lag), _) => format!(
            "lag {} records ({}), unread age {}",
            lag.records,
            lag.bytes.map_or_else(|| "? bytes".into(), human_bytes),
            r.unread_age.map_or_else(|| "?".into(), human_duration),
        ),
        (None, Some(reason)) => format!("lag unknown ({})", reason.code()),
        (None, None) => "lag unknown".into(),
    };
    format!(
        ", {}next seq {}, {lag}, updated {} ago",
        if report.durable_only {
            "durable-only, "
        } else {
            ""
        },
        report.next_seq,
        r.update_age.map_or_else(|| "?".into(), human_duration)
    )
}

/// Escapes a Prometheus label value.
fn label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

struct Exposition<'a> {
    out: &'a mut String,
    journal: String,
}

impl Exposition<'_> {
    fn family(&mut self, name: &str, help: &str) {
        let _ = writeln!(self.out, "# HELP tickertape_{name} {help}");
        let _ = writeln!(self.out, "# TYPE tickertape_{name} gauge");
    }

    fn value(&mut self, name: &str, reader: Option<&str>, value: impl std::fmt::Display) {
        let reader = reader.map_or_else(String::new, |r| format!(",reader=\"{}\"", label(r)));
        let _ = writeln!(
            self.out,
            "tickertape_{name}{{journal=\"{}\"{reader}}} {value}",
            self.journal
        );
    }

    fn gauge(&mut self, name: &str, help: &str, value: Option<impl std::fmt::Display>) {
        if let Some(value) = value {
            self.family(name, help);
            self.value(name, None, value);
        }
    }

    fn readers(
        &mut self,
        s: &Status,
        name: &str,
        help: &str,
        value: impl Fn(&ReaderStatus) -> Option<String>,
    ) {
        let values: Vec<(String, String)> = s
            .readers
            .iter()
            .filter_map(|r| value(r).map(|v| (r.name.clone(), v)))
            .collect();
        if values.is_empty() {
            return;
        }
        self.family(name, help);
        for (reader, v) in values {
            self.value(name, Some(&reader), v);
        }
    }
}

/// Renders one exposition: a sample with its breaches, or (`failed_at`) a sample that failed, which has only
/// `tickertape_up 0` and the timestamp.
fn render_prometheus(
    out: &mut String,
    journal: &Path,
    sample: Option<(&Status, &[Breach])>,
    failed_at: Option<u64>,
) {
    let mut e = Exposition {
        out,
        journal: label(&journal.to_string_lossy()),
    };
    let flag = |b: bool| u8::from(b);
    let at = sample.map_or(failed_at.unwrap_or(0), |(s, _)| s.sampled_unix_ns);
    e.gauge(
        "status_timestamp_seconds",
        "Wall-clock time of this sample; alert when it stops advancing.",
        Some(at as f64 / 1e9),
    );
    e.gauge(
        "up",
        "1 if this sample succeeded, 0 if it failed (the other metrics are then absent).",
        Some(flag(sample.is_some())),
    );
    let Some((s, breaches)) = sample else {
        return;
    };
    e.gauge(
        "healthy",
        "1 if no health threshold given to tickertape status is breached and no corruption was found.",
        Some(flag(breaches.is_empty())),
    );
    e.gauge(
        "corrupt",
        "1 if the sample found the journal corrupt: the head at invalid bytes, or an unusable checkpoint marker.",
        Some(flag(breaches.iter().any(Breach::is_corruption))),
    );
    e.gauge(
        "head_next_seq",
        "Sequence number the next record will get.",
        Some(s.head.position.next_seq),
    );
    e.gauge(
        "head_invalid",
        "1 if the head is at bytes that fail validation.",
        Some(flag(matches!(s.head.end, HeadEnd::Invalid(_)))),
    );
    e.gauge(
        "head_verified",
        "1 if the sample validated every record from the checkpoint to the head.",
        Some(flag(s.head.verified)),
    );
    e.gauge("segments", "Segment files.", Some(s.segments.count));
    e.gauge(
        "segments_bytes",
        "Total size of the segment files.",
        Some(s.segments.total_bytes),
    );
    e.gauge(
        "first_seq",
        "Base sequence number of the oldest segment.",
        Some(s.segments.first_seq),
    );
    e.gauge(
        "segments_before_checkpoint",
        "Segments older than the checkpoint segment (removable by retention).",
        Some(s.segments.before_checkpoint),
    );
    e.gauge(
        "oldest_segment_created_timestamp_seconds",
        "Creation time of the oldest segment.",
        s.segments.oldest_created_unix_ns.map(|ns| ns as f64 / 1e9),
    );
    e.gauge(
        "checkpoint_present",
        "1 if the journal has a valid checkpoint marker slot.",
        Some(flag(s.checkpoint.is_some())),
    );
    e.gauge(
        "checkpoint_invalid",
        "1 if the checkpoint marker points to a missing segment or an impossible offset (the writer refuses the journal).",
        s.checkpoint.map(|c| flag(c.problem.is_some())),
    );
    e.gauge(
        "checkpoint_generation",
        "Generation of the checkpoint marker.",
        s.checkpoint.map(|c| c.marker.generation),
    );
    e.gauge(
        "checkpoint_next_seq",
        "Records below this sequence number are durable.",
        s.checkpoint.map(|c| c.marker.next_seq),
    );
    e.gauge(
        "checkpoint_estimated_age_seconds",
        "Estimated time since the last checkpoint, from the marker's modification time; absent if unreliable.",
        s.checkpoint.and_then(|c| c.estimated_age).map(secs),
    );
    e.gauge(
        "exposure_records",
        "Records a power loss now could remove (not covered by the checkpoint); absent if unknown.",
        s.exposure.records,
    );
    e.gauge(
        "exposure_bytes",
        "Segment-file footprint between the checkpoint position and the head.",
        s.exposure.bytes,
    );
    e.gauge(
        "exposure_estimated_age_seconds",
        "Estimated upper bound on how long uncovered records have been exposed; 0 if none; absent if unknown.",
        s.exposure.estimated_age.map(secs),
    );
    e.gauge(
        "exposure_verified",
        "1 if the records between the checkpoint and the head were validated by this sample.",
        Some(flag(s.exposure.verified)),
    );
    e.gauge(
        "storage_available_bytes",
        "Bytes available to unprivileged users on the journal's filesystem.",
        Some(s.storage.available_bytes),
    );
    e.gauge(
        "storage_size_bytes",
        "Size of the journal's filesystem.",
        Some(s.storage.total_bytes),
    );
    e.gauge(
        "storage_spare_segment",
        "1 if a prepared next segment is allocated (not counted in the estimated rolls).",
        Some(flag(s.storage.spare_segment)),
    );
    e.gauge(
        "storage_estimated_rolls_left",
        "Estimated segment rolls that fit in the available space.",
        Some(s.storage.estimated_rolls_left),
    );
    e.gauge(
        "storage_estimated_time_to_full_seconds",
        "Estimated time until a roll fails for lack of space, at the measured append rate.",
        s.storage.estimated_time_to_full.map(secs),
    );
    e.gauge(
        "append_records_per_second",
        "Records appended per second since the previous sample.",
        s.rates.map(|r| r.records_per_sec),
    );
    e.gauge(
        "writer_alive",
        "1 if the PID in writer.lock exists (best effort).",
        s.writer.alive.map(flag),
    );
    e.gauge(
        "readers_dir_usable",
        "1 if the beacon directory could be listed or doesn't exist, 0 if it couldn't be listed.",
        Some(flag(!matches!(
            s.readers_dir_state,
            BeaconDirState::Unusable(_)
        ))),
    );
    e.readers(
        s,
        "reader_live",
        "1 if the reader's beacon is held by a running process, 0 if its owner died; absent if unknown.",
        |r| match r.state {
            tickertape::status::BeaconState::Live => Some("1".into()),
            tickertape::status::BeaconState::Stale => Some("0".into()),
            _ => None,
        },
    );
    e.readers(
        s,
        "reader_lag_records",
        "Records the reader could read but hasn't (for a durable-only reader, up to the checkpoint), at its last beacon update.",
        |r| r.lag.map(|l| l.records.to_string()),
    );
    e.readers(
        s,
        "reader_lag_bytes",
        "Segment-file footprint between the reader's cursor and what it could read.",
        |r| r.lag.and_then(|l| l.bytes).map(|b| b.to_string()),
    );
    e.readers(
        s,
        "reader_unread_age_seconds",
        "Age of the oldest unread record by its local timestamp; 0 if caught up.",
        |r| r.unread_age.map(|d| secs(d).to_string()),
    );
    e.readers(
        s,
        "reader_update_age_seconds",
        "Time since the reader's beacon was last written.",
        |r| r.update_age.map(|d| secs(d).to_string()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals() {
        assert_eq!(parse_interval("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_interval("2s"), Ok(Duration::from_secs(2)));
        assert_eq!(parse_interval("3m"), Ok(Duration::from_secs(180)));
        assert_eq!(parse_interval("1h"), Ok(Duration::from_secs(3600)));
        for bad in ["0s", "0ms", "ms", "1", "1d", "-1s", "1.5s", "1w", ""] {
            assert!(parse_interval(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn labels_are_escaped() {
        assert_eq!(label(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(label("x\ny"), "x\\ny");
    }

    #[test]
    fn human_units() {
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(128 << 20), "128.0 MiB");
        assert_eq!(human_duration(Duration::from_millis(1500)), "1.500 s");
        assert_eq!(human_duration(Duration::from_secs(90)), "1.5 min");
        assert_eq!(human_duration(Duration::from_secs(5400)), "1.5 h");
        assert_eq!(human_duration(Duration::from_secs(129_600)), "1.5 d");
    }

    /// A symlink planted where the temporary file would go is never followed: the file it points to is untouched,
    /// the output is replaced, and no temporary file remains.
    #[test]
    fn replace_file_never_follows_a_planted_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"precious").unwrap();
        let target = dir.path().join("out.prom");
        for n in 0..3 {
            std::os::unix::fs::symlink(&victim, temp_path(&target, std::process::id(), n)).unwrap();
        }
        replace_file(&target, b"new").unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"precious");
        assert_eq!(fs::read(&target).unwrap(), b"new");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 2 + 3, "{names:?}");
        // The planted links are still symlinks to the victim.
        for n in 0..3 {
            let link = temp_path(&target, std::process::id(), n);
            assert!(
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }

    /// A failed rename removes the temporary file.
    #[test]
    fn replace_file_cleans_up_on_error() {
        let dir = tempfile::tempdir().unwrap();
        // Renaming a file over a non-empty directory fails.
        let target = dir.path().join("out.prom");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("x"), b"").unwrap();
        assert!(replace_file(&target, b"new").is_err());
        let names: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 1);
    }
}
