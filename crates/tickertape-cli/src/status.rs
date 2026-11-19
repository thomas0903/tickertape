//! `tickertape status`: operational metrics as text, JSON or Prometheus text format, and a health check (ADR 0032).

use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, ValueEnum};
use serde::Serialize;
use tickertape::status::{self, BeaconState, Breach, HeadEnd, Sampler, Status, Thresholds};

use crate::{Failure, Result, archive, output, report};

/// Exit code for a breached health threshold.
const UNHEALTHY: u8 = 3;
/// Version of the JSON object; bumped when a field is removed or changes meaning (adding fields doesn't bump it).
const SCHEMA_VERSION: u32 = 1;

pub(crate) const HELP: &str = "Samples the journal without taking the writer lock or writing to it: the writer's head, the checkpoint and what a power loss would remove now (exposure), space left on the volume, the writer process, and readers that publish a beacon (tickertape::status::ReaderBeacon) in <DIR>/readers or --readers-dir. Cost: a scan of the newest segment from the checkpoint (or its start; with --watch only what was appended since the previous sample), one stat per segment, statvfs, and a few small reads per beacon.\n\nJSON schema (schema_version 1; fields may be added): {\"schema_version\":1,\"journal\":string,\"sampled_unix_ns\":u64,\"head\":{\"next_seq\":u64,\"segment_base_seq\":u64,\"offset\":u64,\"segment_size_bytes\":u64,\"end\":\"open\"|\"sealed\"|\"invalid\",\"invalid_reason\":string|null,\"last_local_ts_ns\":u64|null},\"segments\":{\"count\":u64,\"first_seq\":u64,\"total_bytes\":u64,\"oldest_created_unix_ns\":u64|null,\"before_checkpoint\":u64},\"checkpoint\":{\"generation\":u64,\"next_seq\":u64,\"segment_base_seq\":u64,\"offset\":u64,\"written_unix_ns\":u64|null,\"age_s\":number|null,\"clock_skew\":bool}|null,\"exposure\":{\"records\":u64,\"bytes\":u64|null,\"age_s\":number|null,\"oldest_local_ts_ns\":u64|null},\"storage\":{\"available_bytes\":u64,\"total_bytes\":u64,\"segment_size_bytes\":u64,\"spare_segment\":bool,\"rolls_left\":u64,\"time_to_full_s\":number|null},\"rates\":{\"interval_s\":number,\"records_per_s\":number,\"bytes_per_s\":number|null}|null,\"writer\":{\"pid\":u32|null,\"alive\":bool|null},\"readers\":[{\"name\":string,\"state\":\"live\"|\"stale\"|\"invalid\",\"pid\":u32|null,\"next_seq\":u64|null,\"lag_records\":u64|null,\"lag_bytes\":u64|null,\"unread_age_s\":number|null,\"oldest_unread_local_ts_ns\":u64|null,\"updated_unix_ns\":u64|null,\"update_age_s\":number|null}],\"health\":{\"ok\":bool,\"breaches\":[{\"code\":\"head_invalid\"|\"checkpoint_age\"|\"headroom_bytes\"|\"headroom_segments\"|\"reader_lag\",\"reader\":string|null}]}}. With --watch, one object per line.\n\nPrometheus: text exposition format 0.0.4, gauges named tickertape_*, labelled journal=\"<DIR>\" (and reader=\"<name>\"); see docs/cli.md.\n\nExit codes: 0 healthy (or help, or --watch finished); 1 usage, I/O or missing journal; 2 corrupt newest segment header or unsupported format; 3 a health threshold is breached or the head is at invalid bytes (not with --watch, which reports health in each sample).";

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
    /// Unhealthy if records are not covered by a checkpoint older than DUR (integer followed by d, h, m or s), or by none
    #[arg(long, value_name = "DUR", value_parser = archive::parse_duration)]
    max_checkpoint_age: Option<Duration>,
    /// Unhealthy if fewer than B bytes are available on the volume (optional K/M/G/T suffix)
    #[arg(long, value_name = "B", value_parser = archive::parse_bytes)]
    min_headroom: Option<u64>,
    /// Unhealthy if fewer than N more segments fit on the volume
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
    let mut thresholds = Thresholds::default();
    thresholds.max_checkpoint_age = args.max_checkpoint_age;
    thresholds.min_headroom_bytes = args.min_headroom;
    thresholds.min_headroom_segments = args.min_headroom_segments;
    thresholds.max_reader_lag = args.max_reader_lag;
    let output = args
        .output
        .as_deref()
        .map(|path| destination(&args.dir, path))
        .transpose()?;
    let mut sampler = Sampler::new(&args.dir);
    if let Some(readers) = &args.readers_dir {
        sampler = sampler.readers_dir(readers);
    }
    let mut taken = 0u64;
    loop {
        let sample = sampler.sample()?;
        let breaches = sample.breaches(&thresholds);
        let mut text = String::new();
        match format {
            StatusFormat::Text => render_text(&mut text, &sample, &breaches),
            StatusFormat::Json => {
                let json = serde_json::to_string(&JsonStatus::new(&sample, &breaches))
                    .map_err(|e| output::io_error("encode JSON", e))?;
                text.push_str(&json);
                text.push('\n');
            }
            StatusFormat::Prometheus => render_prometheus(&mut text, &sample, &breaches),
        }
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
        taken += 1;
        let Some(interval) = args.watch else {
            if breaches.is_empty() {
                return Ok(());
            }
            let codes: Vec<String> = breaches.iter().map(describe).collect();
            return Err(Failure {
                code: UNHEALTHY,
                message: format!("unhealthy: {}", codes.join(", ")),
            });
        };
        if args.samples.is_some_and(|n| taken >= n) {
            return Ok(());
        }
        std::thread::sleep(interval);
    }
}

fn describe(breach: &Breach) -> String {
    match breach {
        Breach::ReaderLag { name } => format!("reader_lag({name})"),
        other => other.code().to_string(),
    }
}

/// Resolves `path` for `--output`: its directory must exist and be outside the journal.
fn destination(dir: &Path, path: &Path) -> Result<PathBuf> {
    let journal =
        fs::canonicalize(dir).map_err(|e| output::io_error("resolve journal directory", e))?;
    let name = path
        .file_name()
        .ok_or_else(|| Failure::usage("--output must name a file"))?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let parent =
        fs::canonicalize(parent).map_err(|e| output::io_error("resolve output directory", e))?;
    if parent.starts_with(&journal) {
        return Err(Failure::usage(
            "status output must be outside the journal directory",
        ));
    }
    Ok(parent.join(name))
}

/// Writes `bytes` to a temporary file next to `path` and renames it into place, so readers never see a partial file.
fn replace_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(path.file_name().unwrap_or_default());
    tmp_name.push(format!(".tmp.{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    fs::write(&tmp, bytes).map_err(|e| output::io_error("write output", e))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        output::io_error("replace output", e)
    })
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

fn state_name(state: BeaconState) -> &'static str {
    match state {
        BeaconState::Live => "live",
        BeaconState::Stale => "stale",
        BeaconState::Invalid => "invalid",
        _ => "unknown",
    }
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
    written_unix_ns: Option<u64>,
    age_s: Option<f64>,
    clock_skew: bool,
}

#[derive(Serialize)]
struct JsonExposure {
    records: u64,
    bytes: Option<u64>,
    age_s: Option<f64>,
    oldest_local_ts_ns: Option<u64>,
}

#[derive(Serialize)]
struct JsonStorage {
    available_bytes: u64,
    total_bytes: u64,
    segment_size_bytes: u64,
    spare_segment: bool,
    rolls_left: u64,
    time_to_full_s: Option<f64>,
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
struct JsonReader {
    name: String,
    state: &'static str,
    pid: Option<u32>,
    next_seq: Option<u64>,
    lag_records: Option<u64>,
    lag_bytes: Option<u64>,
    unread_age_s: Option<f64>,
    oldest_unread_local_ts_ns: Option<u64>,
    updated_unix_ns: Option<u64>,
    update_age_s: Option<f64>,
}

#[derive(Serialize)]
struct JsonHealth {
    ok: bool,
    breaches: Vec<JsonBreach>,
}

#[derive(Serialize)]
struct JsonBreach {
    code: &'static str,
    reader: Option<String>,
}

impl JsonStatus {
    fn new(s: &Status, breaches: &[Breach]) -> JsonStatus {
        JsonStatus {
            schema_version: SCHEMA_VERSION,
            journal: s.dir.to_string_lossy().into_owned(),
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
                written_unix_ns: c.written_unix_ns,
                age_s: c.age.map(secs),
                clock_skew: c.clock_skew,
            }),
            exposure: JsonExposure {
                records: s.exposure.records,
                bytes: s.exposure.bytes,
                age_s: s.exposure.age.map(secs),
                oldest_local_ts_ns: s.exposure.oldest_local_ts_ns,
            },
            storage: JsonStorage {
                available_bytes: s.storage.available_bytes,
                total_bytes: s.storage.total_bytes,
                segment_size_bytes: s.storage.segment_size,
                spare_segment: s.storage.spare_segment,
                rolls_left: s.storage.rolls_left,
                time_to_full_s: s.storage.time_to_full.map(secs),
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
            readers: s
                .readers
                .iter()
                .map(|r| JsonReader {
                    name: r.name.clone(),
                    state: state_name(r.state),
                    pid: r.pid,
                    next_seq: r.position.map(|p| p.next_seq),
                    lag_records: r.lag.map(|l| l.records),
                    lag_bytes: r.lag.and_then(|l| l.bytes),
                    unread_age_s: r.unread_age.map(secs),
                    oldest_unread_local_ts_ns: r.oldest_unread_local_ts_ns,
                    updated_unix_ns: r.updated_unix_ns,
                    update_age_s: r.update_age.map(secs),
                })
                .collect(),
            health: JsonHealth {
                ok: breaches.is_empty(),
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

// Writing to a String never fails, so the `fmt::Result`s below are ignored.
fn render_text(out: &mut String, s: &Status, breaches: &[Breach]) {
    let _ = writeln!(out, "journal     {}", s.dir.display());
    let end = match s.head.end {
        HeadEnd::Invalid(reason) => format!("invalid: {reason}"),
        other => end_name(other).to_string(),
    };
    let _ = writeln!(
        out,
        "head        next seq {}, segment {} offset {} ({end}), last local ts {}",
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
            let _ = writeln!(
                out,
                "checkpoint  generation {}, next seq {}, written {} ({} ago){}",
                c.marker.generation,
                c.marker.next_seq,
                opt_ts(c.written_unix_ns),
                c.age.map_or_else(|| "?".into(), human_duration),
                if c.clock_skew {
                    ", modification time is in the future"
                } else {
                    ""
                }
            );
        }
    }
    let _ = writeln!(
        out,
        "exposure    {} records, {}, exposed for up to {}",
        s.exposure.records,
        s.exposure
            .bytes
            .map_or_else(|| "? bytes".into(), human_bytes),
        s.exposure
            .age
            .map_or_else(|| "unknown (no checkpoint)".into(), human_duration)
    );
    let _ = writeln!(
        out,
        "storage     {} available of {}, {} more segments of {}{}{}",
        human_bytes(s.storage.available_bytes),
        human_bytes(s.storage.total_bytes),
        s.storage.rolls_left,
        human_bytes(s.storage.segment_size),
        if s.storage.spare_segment {
            " (one prepared)"
        } else {
            ""
        },
        s.storage.time_to_full.map_or_else(String::new, |t| format!(
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
    if s.readers.is_empty() {
        let _ = writeln!(out, "readers     none");
    }
    for (i, r) in s.readers.iter().enumerate() {
        let label = if i == 0 { "readers" } else { "" };
        let detail = match r.lag {
            None => String::new(),
            Some(lag) => format!(
                ", lag {} records ({}), unread age {}, updated {} ago",
                lag.records,
                lag.bytes.map_or_else(|| "? bytes".into(), human_bytes),
                r.unread_age.map_or_else(|| "?".into(), human_duration),
                r.update_age.map_or_else(|| "?".into(), human_duration)
            ),
        };
        let _ = writeln!(
            out,
            "{label:<11} {} {}{detail}",
            r.name,
            state_name(r.state)
        );
    }
    if breaches.is_empty() {
        let _ = writeln!(out, "health      ok");
    } else {
        let codes: Vec<String> = breaches.iter().map(describe).collect();
        let _ = writeln!(out, "health      UNHEALTHY: {}", codes.join(", "));
    }
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
        value: impl Fn(&status::ReaderStatus) -> Option<String>,
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

fn render_prometheus(out: &mut String, s: &Status, breaches: &[Breach]) {
    let mut e = Exposition {
        out,
        journal: label(&s.dir.to_string_lossy()),
    };
    let flag = |b: bool| u8::from(b);
    e.gauge(
        "status_timestamp_seconds",
        "Wall-clock time of this sample.",
        Some(s.sampled_unix_ns as f64 / 1e9),
    );
    e.gauge(
        "healthy",
        "1 if no health threshold given to tickertape status is breached and the head is valid.",
        Some(flag(breaches.is_empty())),
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
        "1 if the journal has a valid checkpoint marker.",
        Some(flag(s.checkpoint.is_some())),
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
        "checkpoint_age_seconds",
        "Time since the checkpoint marker was written (its modification time).",
        s.checkpoint.and_then(|c| c.age).map(secs),
    );
    e.gauge(
        "exposure_records",
        "Records a power loss now could remove (not covered by the checkpoint).",
        Some(s.exposure.records),
    );
    e.gauge(
        "exposure_bytes",
        "Segment bytes between the checkpoint position and the head.",
        s.exposure.bytes,
    );
    e.gauge(
        "exposure_age_seconds",
        "Upper bound on how long uncovered records have been exposed; 0 if none.",
        s.exposure.age.map(secs),
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
        "storage_rolls_left",
        "More segment rolls that fit in the available space.",
        Some(s.storage.rolls_left),
    );
    e.gauge(
        "storage_time_to_full_seconds",
        "Estimated time until a roll fails for lack of space, at the measured append rate.",
        s.storage.time_to_full.map(secs),
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
    e.readers(
        s,
        "reader_live",
        "1 if the reader's beacon is held by a running process, 0 if its owner died.",
        |r| match r.state {
            BeaconState::Live => Some("1".into()),
            BeaconState::Stale => Some("0".into()),
            _ => None,
        },
    );
    e.readers(
        s,
        "reader_lag_records",
        "Records committed but not yet read, at the reader's last beacon update.",
        |r| r.lag.map(|l| l.records.to_string()),
    );
    e.readers(
        s,
        "reader_lag_bytes",
        "Segment bytes between the reader's cursor and the head.",
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
}
