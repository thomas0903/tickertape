//! Cross-process visible latency, corrected for coordinated omission by scheduling
//! every record at t0 + floor(i * 1e9 / rate) on the host realtime clock. The intended
//! timestamp is stored in local_ts_ns. Backlog is never skipped or rebased; the
//! reader records now - intended after validation, excluding only warmup records.
//! This measures visibility, not durability. Optional cold mode measures replay
//! throughput after a best-effort privileged cache purge, never live latency.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use serde_json::{Value, json};
use tickertape::{Backoff, NewRecord, Reader, ReaderConfig, Writer, WriterConfig, clock};
use tickertape_bench::{
    Options, Result, fresh_directory, histogram_json, intended_ns, print_histogram,
    segment_capacity,
};

const HELP: &str = "tt-latency: cross-process writer-to-reader visible latency

Usage: tt-latency run --dir <DIR> --rate <records/s> --count <N>
                     --payload <bytes> --reader safe|mapped
                     [--warmup <N>] [--segment-size <bytes>] [--json] [--cold]
                     [--wait spin|default|block|sleep]
       tt-latency --help

--dir must be new or empty; data is retained. --count includes --warmup (default 0),
which must be smaller than count. Segment size defaults to 134217728 (128 MiB).
The parent spawns itself as writer and reader, waits for both to be ready, and
releases a shared future start time.

--wait sets how the reader waits for the next record (default spin):
  spin     Backoff::busy_spin(), one core at 100 %
  default  ReaderConfig::default(): 100 spins, 10 yields, then block on the notify file
  block    no spins or yields: block on the notify file at once (kernel wake-up only)
  sleep    Backoff::default() with notify off: spins, yields, then 10 us..1 ms sleeps

Coordinated omission: record i is scheduled at t0 + floor(i * 1e9 / rate) using
tickertape::clock::realtime_ns() on one host. local_ts_ns stores the INTENDED time,
not the actual send time. Sends are never skipped or rebased when behind; reader
latency includes backlog. Warmup records use the same schedule but are excluded
from the latency histogram. HdrHistogram has 3 significant figures, in ns; output
is in microseconds. Visibility is measured after reader validation, not durability.
Writer rate covers first append start through last append completion, including
scheduled waits; late_records counts sends starting >1 us after their intended time
(including warmup). Wall-clock jumps can distort results; backwards observations
fail. No core pinning is performed. The writer busy-waits for its schedule; with
--wait spin the reader also busy-polls.

--cold: writer pre-writes, checkpoints and exits before a fresh reader is spawned.
Tries Linux sudo -n sh -c 'echo 3 > /proc/sys/vm/drop_caches' or macOS sudo -n purge.
This optional system-wide operation needs root; eviction is best effort. On failure,
prints 'cold cache unavailable (needs root)' and exits 0 WITHOUT a result (also with
--json). Measures replay records/s and decimal payload MB/s, including reader open
and validation, excluding process startup. No warmup or latency histogram in this
mode. Normal runs need neither root nor network. CI output is smoke data only.

Internal child commands: writer|reader with the same options and --control <DIR>.
Children have bounded deadlines; the parent terminates and reaps peers on failure.";

#[derive(Debug)]
struct Config {
    dir: PathBuf,
    rate: u64,
    count: u64,
    payload: usize,
    reader: String,
    wait: String,
    warmup: u64,
    segment_size: u64,
    json: bool,
    cold: bool,
    control: Option<PathBuf>,
    timeout: Duration,
}

impl Config {
    fn parse(args: &[String], child: bool) -> Result<Self> {
        let mut values = vec![
            "--dir",
            "--rate",
            "--count",
            "--payload",
            "--reader",
            "--warmup",
            "--segment-size",
            "--wait",
        ];
        if child {
            values.push("--control");
        }
        let options = Options::parse(args, &values, &["--json", "--cold"])?;
        let rate = options.number("--rate", None)?;
        let count = options.number("--count", None)?;
        let warmup = options.number("--warmup", Some(0))?;
        if rate == 0 || count == 0 || warmup >= count {
            return Err("rate and count must be positive; warmup must be less than count".into());
        }
        let reader = options.required("--reader")?.to_owned();
        if reader != "safe" && reader != "mapped" {
            return Err("reader must be safe or mapped".into());
        }
        let wait = options.optional("--wait").unwrap_or("spin").to_owned();
        if !["spin", "default", "block", "sleep"].contains(&wait.as_str()) {
            return Err("wait must be spin, default, block or sleep".into());
        }
        let payload = options.number("--payload", None)?.try_into()?;
        let segment_size =
            options.number("--segment-size", Some(WriterConfig::DEFAULT_SEGMENT_SIZE))?;
        segment_capacity(segment_size, payload)?;
        // Validate the last schedule timestamp and monotonic deadline before touching disk.
        intended_ns(clock::realtime_ns(), count - 1, rate)?;
        let timeout = Duration::from_secs((count / rate).checked_add(121).ok_or("run too long")?);
        Instant::now().checked_add(timeout).ok_or("run too long")?;
        let cold = options.flag("--cold");
        if cold && warmup != 0 {
            return Err("--cold does not support --warmup".into());
        }
        let control = options.path("--control");
        if child && control.is_none() {
            return Err("internal child needs --control".into());
        }
        Ok(Self {
            dir: options.required("--dir")?.into(),
            rate,
            count,
            payload,
            reader,
            wait,
            warmup,
            segment_size,
            json: options.flag("--json"),
            cold,
            control,
            timeout,
        })
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
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn spawn(role: &str, args: &[String], control: &Path) -> Result<Process> {
    Ok(Process(
        Command::new(std::env::current_exe()?)
            .arg(role)
            .args(args)
            .arg("--control")
            .arg(control)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?,
    ))
}

fn read_json(path: PathBuf) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn purge_cache() -> bool {
    let mut command = Command::new("sudo");
    command.arg("-n");
    if cfg!(target_os = "linux") {
        command.args(["sh", "-c", "echo 3 > /proc/sys/vm/drop_caches"]);
    } else if cfg!(target_os = "macos") {
        command.arg("purge");
    } else {
        return false;
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn run(config: &Config, args: &[String]) -> Result<()> {
    fresh_directory(&config.dir)?;
    let control = tempfile::tempdir()?;
    let deadline = Instant::now() + config.timeout;
    let mut writer = spawn("writer", args, control.path())?;
    let reader_result = if config.cold {
        wait_for(|| writer.check(), deadline)?;
        if !purge_cache() {
            println!("cold cache unavailable (needs root)");
            return Ok(());
        }
        let mut reader = spawn("reader", args, control.path())?;
        wait_for(|| reader.check(), Instant::now() + config.timeout)?;
        read_json(control.path().join("reader.json"))?
    } else {
        wait_for(
            || {
                writer.check()?;
                Ok(control.path().join("writer.ready").exists())
            },
            deadline,
        )?;
        let mut reader = spawn("reader", args, control.path())?;
        wait_for(
            || {
                writer.check()?;
                reader.check()?;
                Ok(control.path().join("reader.ready").exists())
            },
            deadline,
        )?;
        let t0 = clock::realtime_ns()
            .checked_add(50_000_000)
            .ok_or("start time overflow")?;
        fs::write(control.path().join("start.tmp"), t0.to_string())?;
        fs::rename(
            control.path().join("start.tmp"),
            control.path().join("start"),
        )?;
        wait_for(
            || {
                let w = writer.check()?;
                let r = reader.check()?;
                Ok(w && r)
            },
            deadline,
        )?;
        read_json(control.path().join("reader.json"))?
    };
    let mut result = json!({"mode": if config.cold { "cold_replay" } else { "visible_latency" },
        "count": config.count, "rate": config.rate, "payload": config.payload, "reader": config.reader,
        "wait": config.wait, "warmup": config.warmup, "segment_size": config.segment_size});
    if config.cold {
        result["replay"] = reader_result;
    } else {
        result["latency"] = reader_result;
        result["writer"] = read_json(control.path().join("writer.json"))?;
        result["coordinated_omission"] = json!("intended timestamps; no skipped sends or rebasing");
    }
    if config.json {
        println!("{result}");
    } else {
        println!(
            "mode={} count={} rate={} records/s payload={} B reader={} wait={} warmup={} segment_size={} B",
            result["mode"],
            config.count,
            config.rate,
            config.payload,
            config.reader,
            config.wait,
            config.warmup,
            config.segment_size
        );
        if config.cold {
            println!(
                "Replay: {} records/s, {} MB/s (decimal payload), includes open and validation; best-effort cold cache",
                result["replay"]["records_per_s"], result["replay"]["payload_mb_per_s"]
            );
        } else {
            print_histogram("Visible latency", &result["latency"]);
            println!(
                "Writer: {} records/s achieved, {} late records (>1 µs, includes warmup)",
                result["writer"]["achieved_records_per_s"], result["writer"]["late_records"]
            );
            println!(
                "Coordinated omission: intended timestamps; no skipped sends or rebasing. Same-host wall clock."
            );
        }
    }
    Ok(())
}

fn start_time(control: &Path, deadline: Instant) -> Result<u64> {
    wait_for(|| Ok(control.join("start").exists()), deadline)?;
    Ok(fs::read_to_string(control.join("start"))?.parse()?)
}

fn writer(config: &Config) -> Result<()> {
    let control = config.control.as_ref().ok_or("missing control directory")?;
    let mut writer = Writer::open(
        &config.dir,
        WriterConfig::default().segment_size(config.segment_size),
    )?;
    let payload = vec![0x5a; config.payload];
    let deadline = Instant::now() + config.timeout;
    let t0 = if config.cold {
        0
    } else {
        fs::write(control.join("writer.ready"), [])?;
        start_time(control, deadline)?
    };
    let mut late = 0;
    let mut started = None;
    for i in 0..config.count {
        let intended = if config.cold {
            0
        } else {
            intended_ns(t0, i, config.rate)?
        };
        let sent = loop {
            let now = clock::realtime_ns();
            if config.cold || now >= intended {
                break now;
            }
            std::hint::spin_loop();
        };
        if !config.cold && sent - intended > 1000 {
            late += 1;
        }
        started.get_or_insert_with(Instant::now);
        writer.append(&NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns: intended,
            payload: &payload,
        })?;
    }
    let elapsed = started.ok_or("no records sent")?.elapsed().as_secs_f64();
    if config.cold {
        writer.checkpoint()?;
    }
    fs::write(
        control.join("writer.json"),
        json!({"achieved_records_per_s": config.count as f64 / elapsed,
        "late_records": late, "count": config.count})
        .to_string(),
    )?;
    Ok(())
}

fn reader(config: &Config) -> Result<()> {
    let control = config.control.as_ref().ok_or("missing control directory")?;
    let start = Instant::now();
    let deadline = start + config.timeout;
    let reader_config = match config.wait.as_str() {
        "spin" => ReaderConfig::default()
            .backoff(Backoff::busy_spin())
            .notify(false),
        "block" => {
            let mut backoff = Backoff::default();
            backoff.spins = 0;
            backoff.yields = 0;
            ReaderConfig::default().backoff(backoff)
        }
        "sleep" => ReaderConfig::default().notify(false),
        _ => ReaderConfig::default(),
    };
    let mut reader = if config.reader == "mapped" {
        // SAFETY: only the tickertape writer modifies the journal; the run owns a fresh directory.
        unsafe { Reader::open_mapped(&config.dir, reader_config)? }
    } else {
        Reader::open(&config.dir, reader_config)?
    };
    let mut histogram = Histogram::<u64>::new(3)?;
    if !config.cold {
        fs::write(control.join("reader.ready"), [])?;
        start_time(control, deadline)?;
    }
    for expected in 0..config.count {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("reader timed out".into());
        }
        let record = reader
            .next_timeout(Some(remaining))?
            .ok_or("reader timed out waiting for a record")?;
        let now = if config.cold { 0 } else { clock::realtime_ns() };
        if record.seq != expected || record.payload.len() != config.payload {
            return Err("record sequence or payload length mismatch".into());
        }
        std::hint::black_box(record.payload);
        if !config.cold && expected >= config.warmup {
            let latency = now
                .checked_sub(record.local_ts_ns)
                .ok_or("wall clock moved backwards; invalid latency run")?;
            histogram.record(latency)?;
        }
    }
    let result = if config.cold {
        let seconds = start.elapsed().as_secs_f64();
        json!({"count": config.count, "seconds": seconds, "records_per_s": config.count as f64 / seconds,
            "payload_mb_per_s": config.count as f64 * config.payload as f64 / seconds / 1_000_000.0})
    } else {
        histogram_json(&histogram)
    };
    fs::write(control.join("reader.json"), result.to_string())?;
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
        eprintln!("tt-latency: {error}");
        std::process::exit(1);
    }
}
