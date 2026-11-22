//! Sustained workloads (ADR 0026): one writer process and N reader processes for a fixed duration, with steady,
//! bursty or unpaced traffic, periodic checkpoints, segment rolls, optional writer retention, safe and mapped,
//! visible and durable-only readers, and readers that fall behind. Paced schedules store each record's intended
//! send time in `local_ts_ns` and never skip or rebase sends, as `tt-latency` does. Per-run JSON plus a
//! per-second, wall-clock time series locate stalls in time.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use serde_json::{Value, json};
use tickertape::retention::{Action, Policy};
use tickertape::{Error, HelperCpus, NewRecord, Reader, Writer, WriterConfig, clock};
use tickertape_bench::sustained::{
    CpuSnapshot, PositionFile, ReaderSpec, Schedule, Seconds, SeriesFile, WaitMode, describe_slot,
    format_duration, free_bytes, lag_json, lag_records, merge_series, parse_cpu_list,
    parse_duration, pin_current_thread, quantiles_json, record_bytes, total_ram_bytes,
};
use tickertape_bench::{Options, Result, elapsed_ns, fresh_directory, segment_capacity};

/// Every payload byte the writer sends; a reader that reports a corrupt slot checks the file against it.
const PAYLOAD_BYTE: u8 = 0x5a;

const HELP: &str = "tt-sustained: sustained writer + readers workload (ADR 0026)

Usage: tt-sustained run --dir <DIR> --out <DIR> (--rate <N> | --burst-rate <N> | --unpaced) [options]
       tt-sustained --help

Directories:
  --dir <DIR>             journal directory on the filesystem under test; new or empty, data is retained
  --out <DIR>             result directory (new or empty): result.json, series.jsonl, per-process series

Traffic (exactly one):
  --rate <N>              steady: record i at t0 + floor(i * 1e9 / N)
  --burst-rate <N>        bursty: bursts at N records/s for --burst-duty percent (default 10) of every
                          --burst-period (default 100ms); mean rate = N * duty / 100
  --unpaced               as fast as possible, to find sustainable throughput

Length:
  --duration <D>          measured duration (default 10m); units ns, us, ms, s, m, h (bare number: seconds)
  --warmup <D>            schedule time before the measured part, excluded from latency, lag and checkpoint
                          statistics but included in counts, rates and the series (default 0s)
  --overrun-limit <D>     a paced writer that falls this far behind its schedule end stops sending and reports
                          the unsent records (default 60s)
  --drain-timeout <D>     how long non-lagging readers may take to catch up after the writer finishes
                          (default 60s); undrained readers are reported, not hidden

Writer:
  --payload <B>           payload bytes per record (default 64)
  --segment-size <B>      segment size (default 134217728 = 128 MiB); smaller sizes roll more often
  --checkpoint-every <D>  checkpoint on a fixed wall-clock grid, or
  --checkpoint-records <N> checkpoint after every N records (default: no checkpoints)
  --checkpoint-mode <M>   how periodic checkpoints run: blocking (Writer::checkpoint on the writer thread, the
                          default) or background (Writer::request_checkpoint, ADR 0038: the writer thread only
                          requests, the syncs run on the writer's checkpoint thread while appends continue)
  --retain-segments <N>   writer retention: delete the oldest segments beyond N (needs checkpoints)
  --retain-bytes <B>      writer retention: delete the oldest segments beyond B bytes (needs checkpoints)
  --no-prefault           WriterConfig::prefault(false)
  --no-early-writeback    WriterConfig::early_writeback(false): no sync_file_range behind the writer (Linux;
                          no effect elsewhere)
  --helper-cpus <C>       WriterConfig::helper_cpus for the writer's helper threads (checkpoint, prefault,
                          write-back): process (the default: every CPU the cpuset allows), inherited (the
                          creating thread's mask, so a pinned writer's checkpoint thread shares its CPU), or a
                          CPU list such as 4 or 4,5 (Linux; no effect elsewhere)

Readers:
  --readers <LIST>        comma-separated reader specs (default mapped; none for no readers):
                          kind[:option]... with kind safe (pread) or mapped (zero-copy), options
                            durable             ReaderConfig::durable_only (needs checkpoints)
                            wait=spin|default|block|sleep  (as tt-latency --wait)
                            delay=<D>           pause after every record (busy-wait below 1ms, else sleep)
                            stall=<D>/<D>       pause for the first duration once every second duration
                          a reader with delay or stall is a lagging reader: it stops when the writer finishes
                          and reports how far behind it was
  --wait <MODE>           default wait mode for readers (default spin)
  --lag-interval <D>      how often each reader samples its lag (default 10ms)

Other:
  --cpus <LIST>           pin the writer thread, then reader 0, 1, ... to these CPUs (Linux only; elsewhere
                          prints that pinning is unsupported); helper and sampler threads stay unpinned
  --no-disk-check         start even if the estimated peak disk use exceeds 90 % of free space
  --json                  print the result as one JSON object instead of text

Coordinated omission: paced records carry their intended send time in local_ts_ns; the writer busy-waits for
it, never skips or rebases, and readers measure now - intended, so backlog counts as latency. Unpaced records
carry their actual send start time. Late sends start more than 1 us after their intended time. Append latency
is one Writer::append, timed with the monotonic clock; roll appends are the appends that created a segment.
Checkpoints run on the writer thread, as an application would run them, so they delay later sends; in background
mode the writer thread only requests them and polls Writer::checkpoint_progress after each append while one is in
flight. checkpoint_latency is the writer thread's time in the checkpoint call (blocking: the whole checkpoint;
background: the request). checkpoint_durable_latency is each checkpoint's time from its call to its marker being
durable (background: from the library's completion time, observed at the next poll). checkpoint_pause is, per
checkpoint, the writer thread's longest stretch of work around it: blocking, the call; background, the longest of
the request and every append plus poll while it was in flight. The final checkpoint is always blocking.
A paced writer busy-waits for each intended time; schedule_wait_s is that spinning, included in its CPU time.
Rolls do not sync the new segment on the writer thread (ADR 0038); roll_append_latency shows what a roll still
costs. On Linux the writer's per-second samples carry dirty_bytes and writeback_bytes (/proc/meminfo Dirty and
Writeback, the whole system), and writer.page_cache has their maxima over the measured part: with early write-back
the dirty data a checkpoint finds stays small.
Lag samples are the writer's next sequence (published by the writer about every 1 ms) minus the reader's,
in records and journal bytes. CPU usage comes from getrusage for each process (all threads) and, on Linux,
for the measuring thread alone. The series has one line per wall-clock second (UTC), keyed by when each
operation started in its process. CI runs are functional smoke checks, never results.

Internal child commands: writer|reader with the same options plus --control <DIR> (and --index <N>).";

const SERIES: &str = "series.jsonl";

#[derive(Debug, Clone, Copy, PartialEq)]
enum Checkpoint {
    None,
    Every(Duration),
    Records(u64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CheckpointMode {
    Blocking,
    Background,
}

impl CheckpointMode {
    fn name(self) -> &'static str {
        match self {
            CheckpointMode::Blocking => "blocking",
            CheckpointMode::Background => "background",
        }
    }
}

#[derive(Debug)]
struct Config {
    dir: PathBuf,
    out: PathBuf,
    duration: Duration,
    warmup: Duration,
    overrun: Duration,
    drain: Duration,
    payload: usize,
    record_bytes: u64,
    schedule: Schedule,
    planned: Option<u64>,
    checkpoint: Checkpoint,
    checkpoint_mode: CheckpointMode,
    segment_size: u64,
    capacity: u64,
    retain_segments: Option<usize>,
    retain_bytes: Option<u64>,
    prefault: bool,
    early_writeback: bool,
    helper_cpus: HelperCpus,
    readers: Vec<ReaderSpec>,
    default_wait: WaitMode,
    lag_interval: Duration,
    cpus: Vec<usize>,
    disk_check: bool,
    json: bool,
    control: Option<PathBuf>,
    index: Option<usize>,
}

impl Config {
    fn parse(args: &[String], child: bool) -> Result<Self> {
        let mut values = vec![
            "--dir",
            "--out",
            "--duration",
            "--warmup",
            "--overrun-limit",
            "--drain-timeout",
            "--payload",
            "--rate",
            "--burst-rate",
            "--burst-period",
            "--burst-duty",
            "--segment-size",
            "--checkpoint-every",
            "--checkpoint-records",
            "--checkpoint-mode",
            "--retain-segments",
            "--retain-bytes",
            "--readers",
            "--wait",
            "--lag-interval",
            "--cpus",
            "--helper-cpus",
        ];
        if child {
            values.extend(["--control", "--index"]);
        }
        let options = Options::parse(
            args,
            &values,
            &[
                "--unpaced",
                "--no-prefault",
                "--no-early-writeback",
                "--no-disk-check",
                "--json",
            ],
        )?;
        let duration_of = |key: &str, default: &str| {
            parse_duration(options.optional(key).unwrap_or(default))
                .map_err(|e| format!("{key}: {e}"))
        };
        let duration = duration_of("--duration", "10m")?;
        let warmup = duration_of("--warmup", "0s")?;
        if duration.is_zero() {
            return Err("duration must be positive".into());
        }
        let traffic = [
            options.optional("--rate").is_some(),
            options.optional("--burst-rate").is_some(),
            options.flag("--unpaced"),
        ];
        if traffic.iter().filter(|t| **t).count() != 1 {
            return Err("choose exactly one of --rate, --burst-rate and --unpaced".into());
        }
        if !traffic[1]
            && (options.optional("--burst-period").is_some()
                || options.optional("--burst-duty").is_some())
        {
            return Err("--burst-period and --burst-duty need --burst-rate".into());
        }
        let schedule = if traffic[0] {
            Schedule::steady(options.number("--rate", None)?)?
        } else if traffic[1] {
            Schedule::bursty(
                options.number("--burst-rate", None)?,
                duration_of("--burst-period", "100ms")?,
                options.number("--burst-duty", Some(10))?,
            )?
        } else {
            Schedule::Unpaced
        };
        let total_ns = u64::try_from((warmup + duration).as_nanos())?;
        let planned = schedule.count_within(total_ns);
        if planned == Some(0) {
            return Err("the schedule sends no records within the run".into());
        }
        if let Some(count) = planned {
            schedule.intended_ns(clock::realtime_ns(), count - 1)?;
        }
        let payload: usize = options.number("--payload", Some(64))?.try_into()?;
        let segment_size =
            options.number("--segment-size", Some(WriterConfig::DEFAULT_SEGMENT_SIZE))?;
        let capacity = segment_capacity(segment_size, payload)?;
        let checkpoint = match (
            options.optional("--checkpoint-every"),
            options.optional("--checkpoint-records"),
        ) {
            (Some(_), Some(_)) => {
                return Err("choose one of --checkpoint-every and --checkpoint-records".into());
            }
            (Some(every), None) => {
                let every = parse_duration(every)?;
                if every.is_zero() {
                    return Err("checkpoint interval must be positive".into());
                }
                Checkpoint::Every(every)
            }
            (None, Some(_)) => match options.number("--checkpoint-records", None)? {
                0 => return Err("checkpoint record count must be positive".into()),
                n => Checkpoint::Records(n),
            },
            (None, None) => Checkpoint::None,
        };
        let checkpoint_mode = match options.optional("--checkpoint-mode") {
            None | Some("blocking") => CheckpointMode::Blocking,
            Some("background") => CheckpointMode::Background,
            Some(other) => {
                return Err(
                    format!("--checkpoint-mode {other}: choose blocking or background").into(),
                );
            }
        };
        if checkpoint == Checkpoint::None && options.optional("--checkpoint-mode").is_some() {
            return Err(
                "--checkpoint-mode needs --checkpoint-every or --checkpoint-records".into(),
            );
        }
        let retain_segments = options
            .optional("--retain-segments")
            .map(|_| {
                options
                    .number("--retain-segments", None)
                    .map(usize::try_from)
            })
            .transpose()?
            .transpose()?;
        let retain_bytes = options
            .optional("--retain-bytes")
            .map(|_| options.number("--retain-bytes", None))
            .transpose()?;
        if retain_segments == Some(0) || retain_bytes == Some(0) {
            return Err("retention limits must be positive".into());
        }
        let default_wait = WaitMode::parse(options.optional("--wait").unwrap_or("spin"))?;
        let readers = ReaderSpec::parse_list(
            options.optional("--readers").unwrap_or("mapped"),
            default_wait,
        )?;
        if checkpoint == Checkpoint::None {
            if retain_segments.is_some() || retain_bytes.is_some() {
                return Err("retention runs only after checkpoints: add --checkpoint-every or --checkpoint-records".into());
            }
            if readers.iter().any(|r| r.durable_only) {
                return Err("durable-only readers need checkpoints: add --checkpoint-every or --checkpoint-records".into());
            }
        }
        let lag_interval = duration_of("--lag-interval", "10ms")?;
        if lag_interval < Duration::from_millis(1) {
            return Err("lag interval must be at least 1ms".into());
        }
        let cpus = options
            .optional("--cpus")
            .map(parse_cpu_list)
            .transpose()?
            .unwrap_or_default();
        if !cpus.is_empty() && cpus.len() < 1 + readers.len() {
            return Err(format!(
                "--cpus needs {} CPUs: the writer, then one per reader",
                1 + readers.len()
            )
            .into());
        }
        let helper_cpus = match options.optional("--helper-cpus") {
            None | Some("process") => HelperCpus::Process,
            Some("inherited") => HelperCpus::Inherited,
            Some(list) => HelperCpus::List(parse_cpu_list(list).map_err(|e| {
                format!("--helper-cpus {list}: {e} (process, inherited or a CPU list)")
            })?),
        };
        let control = options.path("--control");
        let index = options
            .optional("--index")
            .map(|_| options.number("--index", None).map(usize::try_from))
            .transpose()?
            .transpose()?;
        if child && control.is_none() {
            return Err("internal child needs --control".into());
        }
        let config = Config {
            dir: options.required("--dir")?.into(),
            out: options.required("--out")?.into(),
            duration,
            warmup,
            overrun: duration_of("--overrun-limit", "60s")?,
            drain: duration_of("--drain-timeout", "60s")?,
            payload,
            record_bytes: record_bytes(payload)?,
            schedule,
            planned,
            checkpoint,
            checkpoint_mode,
            segment_size,
            capacity,
            retain_segments,
            retain_bytes,
            prefault: !options.flag("--no-prefault"),
            early_writeback: !options.flag("--no-early-writeback"),
            helper_cpus,
            readers,
            default_wait,
            lag_interval,
            cpus,
            disk_check: !options.flag("--no-disk-check"),
            json: options.flag("--json"),
            control,
            index,
        };
        // Bounded deadlines must be representable.
        Instant::now()
            .checked_add(config.deadline())
            .ok_or("run too long")?;
        Ok(config)
    }

    fn total(&self) -> Duration {
        self.warmup + self.duration
    }

    /// Longest a healthy run can take after the start time, before the parent gives up.
    fn deadline(&self) -> Duration {
        self.total() + self.overrun + self.drain + Duration::from_secs(120)
    }

    fn retention(&self) -> Option<Policy> {
        if self.retain_segments.is_none() && self.retain_bytes.is_none() {
            return None;
        }
        let mut policy = Policy::default();
        if let Some(n) = self.retain_segments {
            policy = policy.max_segments(n);
        }
        if let Some(b) = self.retain_bytes {
            policy = policy.max_bytes(b);
        }
        Some(policy)
    }

    /// Estimated peak disk use, or None if it can't be bounded (unpaced without retention).
    fn peak_disk_estimate(&self) -> Option<u64> {
        let seg = self.segment_size;
        // Between two checkpoints retention can't run, so a checkpoint interval's worth of data can accumulate.
        let byte_rate = self
            .schedule
            .mean_rate()
            .map(|r| r * self.record_bytes as f64);
        let interval_bytes = match self.checkpoint {
            Checkpoint::Every(every) => byte_rate.map(|r| (r * every.as_secs_f64()) as u64),
            Checkpoint::Records(n) => Some(n.saturating_mul(self.record_bytes)),
            Checkpoint::None => None,
        };
        let bounded = [
            self.retain_segments.map(|n| (n as u64).saturating_mul(seg)),
            self.retain_bytes,
        ]
        .into_iter()
        .flatten()
        .min();
        let all = self.planned.map(|n| {
            n.div_ceil(self.capacity)
                .saturating_add(2)
                .saturating_mul(seg)
        });
        match (bounded, all) {
            (Some(limit), all) => {
                let peak = limit
                    .saturating_add(2 * seg)
                    .saturating_add(interval_bytes.unwrap_or(4 * seg));
                Some(all.map_or(peak, |a| a.min(peak)))
            }
            (None, all) => all,
        }
    }

    fn to_json(&self) -> Value {
        json!({"dir": self.dir, "out": self.out, "duration_s": self.duration.as_secs_f64(),
            "warmup_s": self.warmup.as_secs_f64(), "overrun_limit_s": self.overrun.as_secs_f64(),
            "drain_timeout_s": self.drain.as_secs_f64(), "payload_bytes": self.payload,
            "record_bytes": self.record_bytes, "schedule": self.schedule.to_json(), "planned_records": self.planned,
            "checkpoint": match self.checkpoint {
                Checkpoint::None => json!({"kind": "none"}),
                Checkpoint::Every(d) => json!({"kind": "time", "every_ms": d.as_secs_f64() * 1e3,
                    "mode": self.checkpoint_mode.name()}),
                Checkpoint::Records(n) => json!({"kind": "records", "every_records": n,
                    "mode": self.checkpoint_mode.name()}),
            },
            "segment_size_bytes": self.segment_size, "records_per_segment": self.capacity,
            "retention": {"max_segments": self.retain_segments, "max_bytes": self.retain_bytes,
                "action": if self.retention().is_some() { "delete" } else { "none" }},
            "writer_durability": "visible on append; durable only through checkpoints (crash contract)",
            "prefault": self.prefault, "notify": true, "early_writeback": self.early_writeback,
            "helper_cpus": match &self.helper_cpus {
                HelperCpus::List(list) => json!(list),
                HelperCpus::Inherited => json!("inherited"),
                _ => json!("process"),
            },
            "readers": self.readers.iter().map(ReaderSpec::to_json).collect::<Vec<_>>(),
            "default_wait": self.default_wait.name(), "lag_interval_ms": self.lag_interval.as_secs_f64() * 1e3,
            "writer_position_publish_interval_ms": 1, "cpus": self.cpus,
            "disk_check": self.disk_check})
    }
}

// Ownership guarantees every child is killed and reaped if orchestration fails.
struct Process(Child);
impl Process {
    fn check(&mut self) -> Result<bool> {
        match self.0.try_wait()? {
            Some(status) if !status.success() => {
                Err(format!("child {} failed: {status}", self.0.id()).into())
            }
            Some(_) => Ok(true),
            None => Ok(false),
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for(mut condition: impl FnMut() -> Result<bool>, deadline: Instant) -> Result<()> {
    loop {
        if condition()? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("benchmark timed out".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn spawn(role: &str, args: &[String], control: &Path, index: Option<usize>) -> Result<Process> {
    let mut command = Command::new(std::env::current_exe()?);
    command.arg(role).args(args).arg("--control").arg(control);
    if let Some(index) = index {
        command.arg("--index").arg(index.to_string());
    }
    Ok(Process(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?,
    ))
}

fn read_json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(
        &fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?,
    )?)
}

fn read_lines(path: &Path) -> Result<Vec<Value>> {
    fs::read_to_string(path)?
        .lines()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn host_json(config: &Config) -> Value {
    let kernel = Command::new("uname")
        .arg("-r")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned());
    json!({"os": std::env::consts::OS, "arch": std::env::consts::ARCH, "kernel": kernel,
        "logical_cpus": std::thread::available_parallelism().map(|n| n.get()).ok(),
        "ram_bytes": total_ram_bytes(), "journal_free_bytes_at_start": free_bytes(&config.dir).ok()})
}

fn run(config: &Config, args: &[String]) -> Result<()> {
    fresh_directory(&config.dir)?;
    fresh_directory(&config.out)?;
    let (dir, out) = (config.dir.canonicalize()?, config.out.canonicalize()?);
    if dir.starts_with(&out) || out.starts_with(&dir) {
        return Err("--dir and --out must not contain each other".into());
    }
    let free = free_bytes(&config.dir)?;
    let peak = config.peak_disk_estimate();
    match peak {
        Some(peak) if config.disk_check && peak as f64 > free as f64 * 0.9 => {
            return Err(format!(
                "estimated peak disk use {peak} B exceeds 90 % of the {free} B free on the journal filesystem; \
                 add retention (--retain-segments), shorten the run, or pass --no-disk-check"
            )
            .into());
        }
        None => eprintln!(
            "tt-sustained: warning: unpaced run without retention; disk use is unbounded until the duration ends"
        ),
        _ => {}
    }
    let control = tempfile::tempdir()?;
    PositionFile::create(&control.path().join("writer.pos"))?;
    let setup_deadline = Instant::now() + Duration::from_secs(120);
    let mut writer = spawn("writer", args, control.path(), None)?;
    wait_for(
        || {
            writer.check()?;
            Ok(control.path().join("writer.ready").exists())
        },
        setup_deadline,
    )?;
    let mut readers = Vec::new();
    for index in 0..config.readers.len() {
        readers.push(spawn("reader", args, control.path(), Some(index))?);
    }
    wait_for(
        || {
            writer.check()?;
            for (index, reader) in readers.iter_mut().enumerate() {
                reader.check()?;
                if !control
                    .path()
                    .join(format!("reader-{index}.ready"))
                    .exists()
                {
                    return Ok(false);
                }
            }
            Ok(true)
        },
        setup_deadline,
    )?;
    let t0 = clock::realtime_ns()
        .checked_add(200_000_000)
        .ok_or("start time overflow")?;
    fs::write(control.path().join("start.tmp"), t0.to_string())?;
    fs::rename(
        control.path().join("start.tmp"),
        control.path().join("start"),
    )?;
    let deadline = Instant::now() + config.deadline();
    wait_for(
        || {
            let done = writer.check()?;
            for reader in &mut readers {
                reader.check()?;
            }
            Ok(done)
        },
        deadline,
    )?;
    let drain_deadline = Instant::now() + config.drain + Duration::from_secs(60);
    wait_for(
        || {
            let mut all = true;
            for reader in &mut readers {
                all &= reader.check()?;
            }
            Ok(all)
        },
        drain_deadline,
    )?;
    let writer_result = read_json(&control.path().join("writer.json"))?;
    let mut reader_results = Vec::new();
    for index in 0..config.readers.len() {
        reader_results.push(read_json(
            &control.path().join(format!("reader-{index}.json")),
        )?);
    }
    let count = writer_result["count"]
        .as_u64()
        .ok_or("writer result has no count")?;
    for (index, reader) in reader_results.iter().enumerate() {
        let read = reader["records_read"].as_u64().unwrap_or(u64::MAX);
        let skipped = reader["skipped_by_retention"].as_u64().unwrap_or(u64::MAX);
        let remaining = reader["remaining_records"].as_u64().unwrap_or(u64::MAX);
        if reader["writer_count"].as_u64() != Some(count)
            || read
                .checked_add(skipped)
                .and_then(|v| v.checked_add(remaining))
                != Some(count)
        {
            return Err(format!(
                "reader {index}: records read {read} + skipped {skipped} + remaining {remaining} != written {count}"
            )
            .into());
        }
    }
    let mut processes = vec![read_lines(&config.out.join("writer.series.jsonl"))?];
    for index in 0..config.readers.len() {
        processes.push(read_lines(
            &config.out.join(format!("reader-{index}.series.jsonl")),
        )?);
    }
    let mut writer_result = writer_result;
    writer_result["page_cache"] = page_cache_maxima(
        &processes[0],
        t0.saturating_add(u64::try_from(config.warmup.as_nanos())?) / 1_000_000_000,
    );
    let series = merge_series(&processes);
    let mut file = SeriesFile::create(&config.out.join(SERIES))?;
    for line in &series {
        file.line(line)?;
    }
    file.flush()?;
    let ram = total_ram_bytes();
    let appended = count.saturating_mul(config.record_bytes);
    let result = json!({"mode": "sustained", "tool": "tt-sustained", "tickertape_version": env!("CARGO_PKG_VERSION"),
        "command": std::env::args().collect::<Vec<_>>(), "config": config.to_json(), "host": host_json(config),
        "start_unix_ns": t0, "writer": writer_result, "readers": reader_results,
        "dataset": {"appended_bytes": appended, "ram_bytes": ram, "appended_exceeds_ram": ram.map(|r| appended > r),
            "peak_disk_estimate_bytes": peak, "free_bytes_at_start": free},
        "coordinated_omission": if config.planned.is_some() {
            "intended timestamps; no skipped sends or rebasing"
        } else {
            "unpaced: latency measured from the actual send start"
        },
        "series_file": SERIES, "series_seconds": series.len()});
    fs::write(config.out.join("result.json"), format!("{result}\n"))?;
    if config.json {
        println!("{result}");
    } else {
        print_text(config, &result);
    }
    Ok(())
}

fn stats_line(label: &str, stats: &Value, unit: &str) {
    let get = |key: &str| &stats[format!("{key}_{unit}")];
    println!(
        "{label} ({unit}, count={}): min={} p50={} p90={} p99={} p99.9={} p99.99={} max={}",
        stats.get("count").unwrap_or(&stats["samples"]),
        get("min"),
        get("p50"),
        get("p90"),
        get("p99"),
        get("p99_9"),
        get("p99_99"),
        get("max")
    );
}

fn print_text(config: &Config, result: &Value) {
    let w = &result["writer"];
    println!(
        "tt-sustained: schedule={} duration={} warmup={} payload={} B segment={} B checkpoint={:?} retention={} readers={}",
        result["config"]["schedule"],
        format_duration(config.duration),
        format_duration(config.warmup),
        config.payload,
        config.segment_size,
        config.checkpoint,
        result["config"]["retention"],
        config
            .readers
            .iter()
            .map(ReaderSpec::label)
            .collect::<Vec<_>>()
            .join(",")
    );
    println!(
        "Writer: {} records ({} unsent), {} records/s achieved, {} MB/s payload, {} late (>1 µs), max schedule delay {} µs, stopped by {}",
        w["count"],
        w["unsent_records"],
        w["achieved_records_per_s"],
        w["achieved_payload_mb_per_s"],
        w["late_records"],
        w["max_schedule_delay_us"],
        w["stopped_by"]
    );
    stats_line("Append latency", &w["append_latency"], "us");
    stats_line("Roll append latency", &w["roll_append_latency"], "us");
    println!(
        "Page cache (system): dirty max {} B, write-back max {} B",
        w["page_cache"]["dirty_max_bytes"], w["page_cache"]["writeback_max_bytes"]
    );
    stats_line(
        &format!("Checkpoint latency ({})", w["checkpoint_mode"]),
        &w["checkpoint_latency"],
        "us",
    );
    stats_line(
        "Checkpoint durable latency",
        &w["checkpoint_durable_latency"],
        "us",
    );
    stats_line("Checkpoint pause", &w["checkpoint_pause"], "us");
    println!(
        "Final checkpoint: {} µs; retention removed {} segments; last retention error: {}",
        w["final_checkpoint_us"], w["retention"]["removed_segments"], w["retention"]["last_error"]
    );
    println!("Writer CPU: {}", w["cpu"]);
    for (index, r) in result["readers"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        stats_line(
            &format!("Reader {index} {} {} latency", r["spec"], r["latency_kind"]),
            &r["latency"],
            "us",
        );
        stats_line(&format!("Reader {index} lag"), &r["lag"], "records");
        println!(
            "Reader {index}: read {} skipped {} remaining {} drained {}; max lag {} B; CPU {}",
            r["records_read"],
            r["skipped_by_retention"],
            r["remaining_records"],
            r["drained"],
            r["lag"]["max_bytes"],
            r["cpu"]
        );
        if r["drained"] == false && r["lagging"] == false {
            println!("WARNING: reader {index} did not catch up within the drain timeout");
        }
    }
    println!(
        "Dataset: {} B appended, RAM {} B, exceeds RAM: {}",
        result["dataset"]["appended_bytes"],
        result["dataset"]["ram_bytes"],
        result["dataset"]["appended_exceeds_ram"]
    );
    println!("Coordinated omission: {}", result["coordinated_omission"]);
    println!(
        "Series: {} ({} wall-clock seconds, UTC); result: {}",
        config.out.join(SERIES).display(),
        result["series_seconds"],
        config.out.join("result.json").display()
    );
}

fn start_time(control: &Path, deadline: Instant) -> Result<u64> {
    wait_for(|| Ok(control.join("start").exists()), deadline)?;
    Ok(fs::read_to_string(control.join("start"))?.parse()?)
}

/// What a sampler thread samples besides CPU usage.
enum Sampled {
    /// Publishes the writer's position about every millisecond.
    Writer { position: Arc<AtomicU64> },
    /// Samples lag against the writer's published position.
    Reader {
        own: Arc<AtomicU64>,
        writer_final: Arc<AtomicU64>,
        record_bytes: u64,
    },
}

struct Sampler {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<std::result::Result<Histogram<u64>, String>>,
}

impl Sampler {
    /// Starts the sampler thread. It writes the per-process series file: `ops` lines received from the measuring
    /// thread, and its own `sample` line per wall-clock second (CPU fractions, page faults, position, lag).
    fn start(
        sampled: Sampled,
        file: PositionFile,
        mut series: SeriesFile,
        ops: mpsc::Receiver<Value>,
        interval: Duration,
        start_ns: u64,
        measure_from_ns: u64,
    ) -> Sampler {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("tt-sampler".into())
            .spawn(move || {
            let mut body = move || -> Result<Histogram<u64>> {
                let mut lag = Histogram::<u64>::new(3)?;
                // Nothing happens before the shared start time; the first sample covers its second.
                while clock::realtime_ns() < start_ns && !stopping.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                let mut second = start_ns / 1_000_000_000;
                let mut cpu = CpuSnapshot::process()?;
                let mut lag_max: Option<u64> = None;
                let mut position: u64;
                let mut writer_position = 0;
                loop {
                    let stopped = stopping.load(Ordering::Acquire);
                    let now = clock::realtime_ns();
                    match &sampled {
                        Sampled::Writer { position: shared } => {
                            position = shared.load(Ordering::Acquire);
                            if !stopped {
                                file.publish(position, false)?;
                            }
                        }
                        Sampled::Reader { own, writer_final, .. } => {
                            let writer = file.read()?;
                            if writer.done {
                                writer_final.store(writer.next_seq, Ordering::Release);
                            }
                            position = own.load(Ordering::Acquire);
                            writer_position = writer.next_seq;
                            if !stopped {
                                let sample = lag_records(writer.next_seq, position);
                                if now >= measure_from_ns {
                                    lag.record(sample)?;
                                }
                                lag_max = Some(lag_max.unwrap_or(0).max(sample));
                            }
                        }
                    }
                    while let Ok(line) = ops.try_recv() {
                        series.line(&line)?;
                    }
                    if now / 1_000_000_000 != second || stopped {
                        let snapshot = CpuSnapshot::process()?;
                        let usage = cpu.delta_json(&snapshot);
                        cpu = snapshot;
                        let mut line = json!({"kind": "sample", "second": second, "position": position,
                            "cpu_fraction": usage["cpu_fraction"], "user_fraction": usage["user_fraction"],
                            "system_fraction": usage["system_fraction"], "minor_faults": usage["minor_faults"],
                            "major_faults": usage["major_faults"]});
                        if let Sampled::Writer { .. } = &sampled {
                            let (dirty, writeback) = page_cache_bytes();
                            line["dirty_bytes"] = json!(dirty);
                            line["writeback_bytes"] = json!(writeback);
                        }
                        if let Sampled::Reader { record_bytes, .. } = &sampled {
                            line["writer_position"] = json!(writer_position);
                            line["lag_max_records"] = json!(lag_max);
                            line["lag_max_bytes"] = json!(lag_max.map(|l| l.saturating_mul(*record_bytes)));
                        }
                        series.line(&line)?;
                        series.flush()?;
                        second = now / 1_000_000_000;
                        lag_max = None;
                    }
                    if stopped {
                        // The measuring thread has finished and sent its last lines.
                        while let Ok(line) = ops.try_recv() {
                            series.line(&line)?;
                        }
                        series.flush()?;
                        return Ok(lag);
                    }
                    std::thread::sleep(interval);
                }
            };
            body().map_err(|e| e.to_string())
            })
            .expect("spawn sampler thread");
        Sampler { stop, thread }
    }

    fn finish(self) -> Result<Histogram<u64>> {
        self.stop.store(true, Ordering::Release);
        Ok(self
            .thread
            .join()
            .map_err(|_| "sampler thread panicked")??)
    }
}

fn hist() -> Result<Histogram<u64>> {
    Ok(Histogram::<u64>::new(3)?)
}

/// A background checkpoint requested by the writer loop whose marker is not durable yet.
struct InFlight {
    /// The sequence number it covers up to.
    covers: u64,
    requested: Instant,
    /// Requested after the warm-up.
    measured: bool,
    /// The writer thread's longest stretch of work since the request (the request itself, then append plus poll).
    pause_ns: u64,
}

/// Records the requests a durable marker now covers: their call-to-durable time, from the library's completion time
/// (`Writer::metrics().since_checkpoint`, measured from when the marker became durable), and their longest pause.
fn settle(
    writer: &Writer,
    progress: &tickertape::CheckpointProgress,
    in_flight: &mut VecDeque<InFlight>,
    durable: &mut Histogram<u64>,
    pause: &mut Histogram<u64>,
) -> Result<()> {
    let Some(marker) = progress.durable else {
        return Ok(());
    };
    if in_flight.front().is_none_or(|r| r.covers > marker.next_seq) {
        return Ok(());
    }
    let now = Instant::now();
    let since = writer.metrics().since_checkpoint.unwrap_or_default();
    let durable_at = now.checked_sub(since).unwrap_or(now);
    while let Some(request) = in_flight.front() {
        if request.covers > marker.next_seq {
            break;
        }
        if request.measured {
            durable.record(elapsed_ns(
                durable_at.saturating_duration_since(request.requested),
            )?)?;
            pause.record(request.pause_ns)?;
        }
        in_flight.pop_front();
    }
    Ok(())
}

fn writer(config: &Config) -> Result<()> {
    let control = config.control.as_ref().ok_or("missing control directory")?;
    let mut writer_config = WriterConfig::default()
        .segment_size(config.segment_size)
        .prefault(config.prefault)
        .early_writeback(config.early_writeback)
        .helper_cpus(config.helper_cpus.clone());
    if let Some(policy) = config.retention() {
        writer_config = writer_config.retention(policy, Action::Delete);
    }
    let mut writer = Writer::open(&config.dir, writer_config)?;
    let position_file = PositionFile::open(&control.join("writer.pos"))?;
    let series = SeriesFile::create(&config.out.join("writer.series.jsonl"))?;
    let payload = vec![PAYLOAD_BYTE; config.payload];
    let setup_deadline = Instant::now() + Duration::from_secs(120);
    fs::write(control.join("writer.ready"), [])?;
    let t0 = start_time(control, setup_deadline)?;
    let warmup_ns = u64::try_from(config.warmup.as_nanos())?;
    let total_ns = u64::try_from(config.total().as_nanos())?;
    let overrun_ns = u64::try_from(config.overrun.as_nanos())?;
    let measure_from = t0.saturating_add(warmup_ns);
    let end = t0.saturating_add(total_ns);
    let overrun_end = end.saturating_add(overrun_ns);
    let position = Arc::new(AtomicU64::new(writer.next_seq()));
    let (tx, rx) = mpsc::channel();
    // Start the sampler before pinning: threads inherit the creating thread's CPU mask.
    let sampler = Sampler::start(
        Sampled::Writer {
            position: Arc::clone(&position),
        },
        PositionFile::open(&control.join("writer.pos"))?,
        series,
        rx,
        Duration::from_millis(1),
        t0,
        measure_from,
    );
    let pinned = config
        .cpus
        .first()
        .map(|cpu| pin_current_thread(*cpu))
        .transpose()?;
    let mut emit = |line: Value| {
        let _ = tx.send(line);
    };
    let mut seconds = Seconds::new()?;
    let (mut append, mut roll, mut checkpoint) = (hist()?, hist()?, hist()?);
    let (mut durable, mut pause) = (hist()?, hist()?);
    let (mut late, mut max_delay, mut rolls, mut checkpoints, mut overruns) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    // Background mode: requests whose markers are not durable yet, oldest first.
    let mut in_flight: VecDeque<InFlight> = VecDeque::new();
    let mut requests_while_in_flight = 0u64;
    let mut next_checkpoint = match config.checkpoint {
        Checkpoint::Every(every) => t0.saturating_add(u64::try_from(every.as_nanos())?),
        _ => u64::MAX,
    };
    while clock::realtime_ns() < t0 {
        std::hint::spin_loop();
    }
    let cpu_start = CpuSnapshot::process()?;
    let thread_start = CpuSnapshot::thread()?;
    let mut first: Option<Instant> = None;
    let mut last_end = None;
    let mut sent_count = 0u64;
    let mut schedule_wait_ns = 0u64;
    let stopped_by;
    loop {
        let (intended, sent) = match config.planned {
            Some(planned) => {
                if sent_count == planned {
                    stopped_by = "schedule_end";
                    break;
                }
                let intended = config.schedule.intended_ns(t0, sent_count)?;
                let mut entered = None;
                let sent = loop {
                    let now = clock::realtime_ns();
                    entered.get_or_insert(now);
                    if now >= intended {
                        break now;
                    }
                    std::hint::spin_loop();
                };
                schedule_wait_ns += sent - entered.unwrap_or(sent);
                if sent > overrun_end {
                    stopped_by = "overrun_limit";
                    break;
                }
                (intended, sent)
            }
            None => {
                let sent = clock::realtime_ns();
                if sent >= end {
                    stopped_by = "duration";
                    break;
                }
                (sent, sent)
            }
        };
        let delay = sent - intended;
        let seq = writer.next_seq();
        let is_roll = seq != 0 && seq % config.capacity == 0;
        let start = Instant::now();
        writer.append(&NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns: intended,
            payload: &payload,
        })?;
        let elapsed = start.elapsed();
        let append_ns = elapsed_ns(elapsed)?;
        position.store(seq + 1, Ordering::Release);
        first.get_or_insert(start);
        last_end = Some(start + elapsed);
        sent_count += 1;
        if delay > 1_000 {
            late += 1;
        }
        max_delay = max_delay.max(delay);
        if intended >= measure_from {
            append.record(append_ns)?;
            if is_roll {
                roll.record(append_ns)?;
            }
        }
        rolls += u64::from(is_roll);
        let bucket = seconds.bucket(sent, &mut emit);
        bucket.records += 1;
        bucket.late += u64::from(delay > 1_000);
        bucket.schedule_delay_max_ns = bucket.schedule_delay_max_ns.max(delay);
        bucket.latency.record(append_ns)?;
        if is_roll {
            bucket.rolls += 1;
            bucket.roll_max_ns = bucket.roll_max_ns.max(append_ns);
        }
        if !in_flight.is_empty() {
            let polled = Instant::now();
            let progress = writer.checkpoint_progress()?;
            let work = append_ns.saturating_add(elapsed_ns(polled.elapsed())?);
            for request in &mut in_flight {
                request.pause_ns = request.pause_ns.max(work);
            }
            settle(&writer, &progress, &mut in_flight, &mut durable, &mut pause)?;
        }
        let due = match config.checkpoint {
            Checkpoint::None => false,
            Checkpoint::Every(_) => sent >= next_checkpoint,
            Checkpoint::Records(n) => sent_count % n == 0,
        };
        if due {
            let wall = clock::realtime_ns();
            let start = Instant::now();
            let measured = wall >= measure_from;
            // One clock reading for the call, used for its latency and its pause alike: a second reading would add
            // whatever ran (or preempted the thread) in between to one of them only.
            let ns = match config.checkpoint_mode {
                CheckpointMode::Blocking => {
                    writer.checkpoint()?;
                    let ns = elapsed_ns(start.elapsed())?;
                    if measured {
                        durable.record(ns)?;
                        pause.record(ns)?;
                    }
                    ns
                }
                CheckpointMode::Background => {
                    requests_while_in_flight += u64::from(!in_flight.is_empty());
                    let covers = writer.request_checkpoint()?;
                    let ns = elapsed_ns(start.elapsed())?;
                    in_flight.push_back(InFlight {
                        covers,
                        requested: start,
                        measured,
                        pause_ns: ns,
                    });
                    ns
                }
            };
            checkpoints += 1;
            if measured {
                checkpoint.record(ns)?;
            }
            let bucket = seconds.bucket(wall, &mut emit);
            bucket.checkpoints += 1;
            bucket.checkpoint_max_ns = bucket.checkpoint_max_ns.max(ns);
            if let Checkpoint::Every(every) = config.checkpoint {
                // Keep the fixed grid; count grid points that passed while the writer was busy.
                let every = u64::try_from(every.as_nanos())?;
                next_checkpoint = next_checkpoint.saturating_add(every);
                let now = clock::realtime_ns();
                while next_checkpoint <= now {
                    next_checkpoint = next_checkpoint.saturating_add(every);
                    overruns += 1;
                }
            }
        }
    }
    seconds.finish(&mut emit);
    // Let the background checkpoints in flight finish, so each gets its durable time.
    let drain_deadline = Instant::now() + Duration::from_secs(120);
    while !in_flight.is_empty() {
        let progress = writer.checkpoint_progress()?;
        settle(&writer, &progress, &mut in_flight, &mut durable, &mut pause)?;
        if Instant::now() >= drain_deadline {
            return Err(
                "background checkpoints did not finish within 120 s of the run's end".into(),
            );
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    let final_checkpoint_us = if config.checkpoint == Checkpoint::None {
        None
    } else {
        let start = Instant::now();
        writer.checkpoint()?;
        Some(start.elapsed().as_secs_f64() * 1e6)
    };
    drop(tx);
    let cpu_end = CpuSnapshot::process()?;
    let thread_end = CpuSnapshot::thread()?;
    sampler.finish()?;
    position_file.publish(sent_count, true)?;
    let seconds_active = match (first, last_end) {
        (Some(first), Some(last)) => last.duration_since(first).as_secs_f64(),
        _ => 0.0,
    };
    let rate = (seconds_active > 0.0).then(|| sent_count as f64 / seconds_active);
    let report = writer.retention_report();
    let result = json!({"count": sent_count, "planned_records": config.planned,
        "unsent_records": config.planned.map(|p| p - sent_count), "stopped_by": stopped_by,
        "records_measured": append.len(), "seconds": seconds_active, "achieved_records_per_s": rate,
        "achieved_payload_mb_per_s": rate.map(|r| r * config.payload as f64 / 1e6),
        "intended_records_per_s": config.schedule.mean_rate(),
        "late_records": config.planned.map(|_| late), "max_schedule_delay_us": config.planned.map(|_| max_delay as f64 / 1e3),
        "append_latency": quantiles_json(&append, "us", 1e3), "roll_append_latency": quantiles_json(&roll, "us", 1e3),
        "rolls": rolls, "checkpoints": checkpoints, "checkpoint_grid_overruns": overruns,
        "checkpoint_mode": config.checkpoint_mode.name(),
        "checkpoint_latency": quantiles_json(&checkpoint, "us", 1e3),
        "checkpoint_durable_latency": quantiles_json(&durable, "us", 1e3),
        "checkpoint_pause": quantiles_json(&pause, "us", 1e3),
        "checkpoint_requests_while_in_flight": requests_while_in_flight,
        "final_checkpoint_us": final_checkpoint_us,
        "retention": {"removed_segments": report.removed_segments, "removed_bytes": report.removed_bytes,
            "already_removed_segments": report.already_removed_segments,
            "last_error": writer.last_retention_error().map(|e| e.to_string())},
        "cpu": cpu_start.delta_json(&cpu_end),
        "hot_thread_cpu": thread_start.zip(thread_end).map(|(a, b)| a.delta_json(&b)),
        "schedule_wait_s": schedule_wait_ns as f64 / 1e9,
        "pinned": pinned});
    fs::write(control.join("writer.json"), result.to_string())?;
    Ok(())
}

/// Pauses a lagging reader: busy-waits below 1 ms (sleep granularity is too coarse), sleeps otherwise.
fn pause(duration: Duration) {
    if duration < Duration::from_millis(1) {
        let until = Instant::now() + duration;
        while Instant::now() < until {
            std::hint::spin_loop();
        }
    } else {
        std::thread::sleep(duration);
    }
}

fn reader(config: &Config) -> Result<()> {
    let control = config.control.as_ref().ok_or("missing control directory")?;
    let index = config.index.ok_or("reader needs --index")?;
    let spec = config
        .readers
        .get(index)
        .ok_or("reader index out of range")?;
    let mut reader = if spec.mapped {
        // SAFETY: only the tickertape writer modifies the journal; the run owns a fresh directory.
        unsafe { Reader::open_mapped(&config.dir, spec.reader_config())? }
    } else {
        Reader::open(&config.dir, spec.reader_config())?
    };
    let series = SeriesFile::create(&config.out.join(format!("reader-{index}.series.jsonl")))?;
    let setup_deadline = Instant::now() + Duration::from_secs(120);
    fs::write(control.join(format!("reader-{index}.ready")), [])?;
    let t0 = start_time(control, setup_deadline)?;
    let hard_deadline = Instant::now() + config.deadline();
    let measure_from = t0.saturating_add(u64::try_from(config.warmup.as_nanos())?);
    let own = Arc::new(AtomicU64::new(0));
    let writer_final = Arc::new(AtomicU64::new(u64::MAX));
    let (tx, rx) = mpsc::channel();
    let sampler = Sampler::start(
        Sampled::Reader {
            own: Arc::clone(&own),
            writer_final: Arc::clone(&writer_final),
            record_bytes: config.record_bytes,
        },
        PositionFile::open(&control.join("writer.pos"))?,
        series,
        rx,
        config.lag_interval,
        t0,
        measure_from,
    );
    let pinned = config
        .cpus
        .get(1 + index)
        .map(|cpu| pin_current_thread(*cpu))
        .transpose()?;
    let mut emit = |line: Value| {
        let _ = tx.send(line);
    };
    let mut seconds = Seconds::new()?;
    let mut latency = hist()?;
    let (mut expected, mut read, mut skipped, mut discards) = (0u64, 0u64, 0u64, 0u64);
    let mut drain_until: Option<Instant> = None;
    let mut next_stall = spec
        .stall
        .map(|s| t0.saturating_add(u64::try_from(s.every.as_nanos()).unwrap_or(u64::MAX)));
    let cpu_start = CpuSnapshot::process()?;
    let thread_start = CpuSnapshot::thread()?;
    let drained = loop {
        let final_count = writer_final.load(Ordering::Acquire);
        if final_count != u64::MAX {
            if expected >= final_count {
                break true;
            }
            if spec.lagging() {
                break false;
            }
            if Instant::now() >= *drain_until.get_or_insert_with(|| Instant::now() + config.drain) {
                break false;
            }
        }
        match reader.next_timeout(Some(Duration::from_millis(50))) {
            Ok(Some(record)) => {
                let now = clock::realtime_ns();
                if record.seq != expected || record.payload.len() != config.payload {
                    return Err(format!(
                        "record sequence or payload length mismatch: seq {} expected {expected}",
                        record.seq
                    )
                    .into());
                }
                std::hint::black_box(record.payload);
                let sample = now
                    .checked_sub(record.local_ts_ns)
                    .ok_or("wall clock moved backwards; invalid latency run")?;
                if record.local_ts_ns >= measure_from {
                    latency.record(sample)?;
                }
                let bucket = seconds.bucket(now, &mut emit);
                bucket.records += 1;
                bucket.latency.record(sample)?;
                expected += 1;
                read += 1;
                own.store(expected, Ordering::Release);
                if let Some(delay) = spec.delay {
                    pause(delay);
                }
                if let (Some(stall), Some(next)) = (spec.stall, next_stall.as_mut()) {
                    if now >= *next {
                        pause(stall.pause);
                        let every = u64::try_from(stall.every.as_nanos())?;
                        let after = clock::realtime_ns();
                        while *next <= after {
                            *next = next.saturating_add(every);
                        }
                    }
                }
            }
            Ok(None) => {
                if Instant::now() >= hard_deadline {
                    return Err("reader timed out".into());
                }
            }
            Err(Error::Discarded { .. }) => {
                // Retention removed the segment under this reader. Continue at the oldest record still present
                // and count what it never saw.
                discards += 1;
                loop {
                    match reader.seek(expected) {
                        Ok(()) => break,
                        Err(Error::SeqUnavailable { first, .. }) if first > expected => {
                            let lost = first - expected;
                            skipped += lost;
                            seconds.bucket(clock::realtime_ns(), &mut emit).skipped += lost;
                            expected = first;
                            own.store(expected, Ordering::Release);
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            Err(e) => {
                if let Error::Corrupt {
                    path,
                    offset,
                    reason,
                    ..
                } = &e
                {
                    // Say which reader saw it and what the file holds there now and a little later, so that a
                    // wrong file can be told from a reader that saw the bytes wrong (issue #43).
                    let kind = if spec.mapped { "mapped" } else { "safe" };
                    eprintln!(
                        "tt-sustained: reader {index} ({kind}) expected seq {expected} at {} offset {offset}: {reason}",
                        path.display()
                    );
                    // The reader reads the slot again through its own view (the same mapping for a mapped
                    // reader) until it gets the record, to tell a view that stays wrong from one that catches up.
                    let mut reread = true;
                    for delay_ms in [0u64, 1, 10, 100, 1000] {
                        std::thread::sleep(Duration::from_millis(delay_ms));
                        match describe_slot(path, *offset, config.payload, PAYLOAD_BYTE) {
                            Ok(slot) => eprintln!("tt-sustained: file after {delay_ms} ms: {slot}"),
                            Err(e) => eprintln!("tt-sustained: file after {delay_ms} ms: {e}"),
                        }
                        if reread {
                            let again = match reader.try_next() {
                                Ok(Some(record)) => {
                                    reread = false;
                                    format!("record seq {} (valid)", record.seq)
                                }
                                Ok(None) => "no record".to_string(),
                                Err(e) => e.to_string(),
                            };
                            eprintln!(
                                "tt-sustained: reader {index} reads it again after {delay_ms} ms: {again}"
                            );
                        }
                    }
                }
                return Err(e.into());
            }
        }
    };
    seconds.finish(&mut emit);
    drop(tx);
    let cpu_end = CpuSnapshot::process()?;
    let thread_end = CpuSnapshot::thread()?;
    let lag = sampler.finish()?;
    let final_count = writer_final.load(Ordering::Acquire);
    let final_count = if final_count == u64::MAX {
        PositionFile::open(&control.join("writer.pos"))?
            .read()?
            .next_seq
    } else {
        final_count
    };
    let mut result = spec.to_json();
    let fields = json!({"index": index, "records_read": read, "records_measured": latency.len(),
        "skipped_by_retention": skipped, "discard_events": discards, "writer_count": final_count,
        "remaining_records": final_count.saturating_sub(expected), "drained": drained,
        "latency": quantiles_json(&latency, "us", 1e3), "lag": lag_json(&lag, config.record_bytes),
        "cpu": cpu_start.delta_json(&cpu_end),
        "hot_thread_cpu": thread_start.zip(thread_end).map(|(a, b)| a.delta_json(&b)), "pinned": pinned});
    if let (Value::Object(target), Value::Object(source)) = (&mut result, fields) {
        target.extend(source);
    }
    fs::write(
        control.join(format!("reader-{index}.json")),
        result.to_string(),
    )?;
    Ok(())
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP}");
        return;
    }
    let result = (|| {
        let role = args[0].as_str();
        if !["run", "writer", "reader"].contains(&role) {
            return Err("expected run, writer or reader; see --help".into());
        }
        let config = Config::parse(&args[1..], role != "run")?;
        match role {
            "run" => run(&config, &args[1..]),
            "writer" => writer(&config),
            _ => reader(&config),
        }
    })();
    if let Err(error) = result {
        eprintln!("tt-sustained: {error}");
        std::process::exit(1);
    }
}

/// The system's dirty and under-write-back page cache in bytes (`/proc/meminfo` `Dirty` and `Writeback`; Linux only).
fn page_cache_bytes() -> (Option<u64>, Option<u64>) {
    let Ok(meminfo) = fs::read_to_string("/proc/meminfo") else {
        return (None, None);
    };
    let field = |name: &str| {
        meminfo
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
            .and_then(|kib| kib.checked_mul(1024))
    };
    (field("Dirty:"), field("Writeback:"))
}

/// The largest `dirty_bytes` and `writeback_bytes` of the writer's samples from `from_second` on (null without
/// samples, as on macOS).
fn page_cache_maxima(writer_lines: &[Value], from_second: u64) -> Value {
    let max = |field: &str| {
        writer_lines
            .iter()
            .filter(|l| {
                l["kind"] == "sample" && l["second"].as_u64().is_some_and(|s| s >= from_second)
            })
            .filter_map(|l| l[field].as_u64())
            .max()
    };
    json!({"dirty_max_bytes": max("dirty_bytes"), "writeback_max_bytes": max("writeback_bytes"),
        "source": "/proc/meminfo Dirty and Writeback (whole system), sampled once per second by the writer process"})
}
