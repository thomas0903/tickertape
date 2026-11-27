//! Common process protocol for the Rust comparison adapters. Every reader validates
//! ordering and record length. Payload MB/s excludes the adapter's 16-byte envelope.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use serde_json::{Value, json};
use tickertape::{HelperCpus, NewRecord, Reader, ReaderConfig, Writer, WriterConfig, clock};
use tickertape_bench::affinity::{pin_or_fail, report, thread_id};
use tickertape_bench::sustained::parse_cpu_list;
use tickertape_bench::{Options, Result, histogram_json, intended_ns};

const SEGMENT_SIZE: u64 = 134_217_728;

/// Records `append_steady` writes before its timer starts, the same for every tool (Harness.java has the same value).
const PRIMING_RECORDS: u64 = 1_000;

/// Application payload sizes the adapters accept (Harness.java has the same list). 256 B is for one opt-in scenario
/// of the sustained mode; the short mode uses 64 and 1,024.
const PAYLOADS: [usize; 3] = [64, 256, 1024];

/// Percentiles of the live reader's `percentiles_us` (Harness.java has the same list).
const LADDER: [(&str, f64); 10] = [
    ("50", 0.5),
    ("90", 0.9),
    ("97", 0.97),
    ("99", 0.99),
    ("99.7", 0.997),
    ("99.9", 0.999),
    ("99.97", 0.9997),
    ("99.99", 0.9999),
    ("99.997", 0.99997),
    ("99.999", 0.99999),
];

/// The percentile ladder of a latency histogram in µs, with the maximum; `null` values when it is empty.
fn ladder(hist: &Histogram<u64>) -> Value {
    let stat = |v: u64| {
        if hist.is_empty() {
            Value::Null
        } else {
            json!(v as f64 / 1000.0)
        }
    };
    let mut map = serde_json::Map::new();
    for (name, quantile) in LADDER {
        map.insert(name.into(), stat(hist.value_at_quantile(quantile)));
    }
    map.insert("max".into(), stat(hist.max()));
    Value::Object(map)
}

/// The live reader's latency result: the common histogram fields plus `percentiles_us`.
fn live_histogram(hist: &Histogram<u64>) -> Value {
    let mut result = histogram_json(hist);
    result["percentiles_us"] = ladder(hist);
    result
}

fn wait_start(control: &Path, role: &str) -> Result<u64> {
    fs::write(control.join(format!("{role}.ready")), [])?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while !control.join("start").exists() {
        if Instant::now() > deadline {
            return Err("start barrier timed out".into());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(fs::read_to_string(control.join("start"))?.parse()?)
}

fn validate(data: &[u8], expected: u64, payload: usize) -> Result<u64> {
    if data.len() != payload + 16 || u64::from_le_bytes(data[..8].try_into()?) != expected {
        return Err("record sequence or length mismatch".into());
    }
    std::hint::black_box(&data[16..]);
    Ok(u64::from_le_bytes(data[8..16].try_into()?))
}

/// With `--send-start on`, the reader's clock read of the last measured record, for the histogram from the send start
/// (0: the record was not measured). A static, not a local, so that `observe` captures nothing more in `run::<false>`;
/// only the reader thread uses it.
static LAST_READ: AtomicU64 = AtomicU64::new(0);

/// The send start that a writer with `--send-start on` stored in the first 8 application bytes.
fn send_start(application: &[u8]) -> Result<u64> {
    Ok(u64::from_le_bytes(
        application.get(..8).ok_or("short payload")?.try_into()?,
    ))
}

// Only the plain-file adapter uses this decoder. EOF may split either the header
// or body, so retain partial reads until publication is complete; never seek back.
struct FileReader {
    file: File,
    frame: Vec<u8>,
    filled: usize,
}
impl FileReader {
    fn poll(&mut self) -> Result<Option<&[u8]>> {
        while self.filled < self.frame.len() {
            match self.file.read(&mut self.frame[self.filled..]) {
                Ok(0) => return Ok(None),
                Ok(n) => self.filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        let len = u32::from_le_bytes(self.frame[..4].try_into()?) as usize;
        if len != self.frame.len() - 4 {
            return Err("bad file frame length".into());
        }
        self.filled = 0;
        Ok(Some(&self.frame[4..]))
    }
}

// One value per process; boxing the writer would only add an indirection to the measured path.
#[allow(clippy::large_enum_variant)]
enum Sink {
    Tape(Writer),
    File(File),
    Yaque(yaque::Sender),
    Queue(queue_file::QueueFile),
}
impl Sink {
    /// `helper_cpus`: where the tool's own threads go. Only tickertape has any in the writer, and its default would
    /// leave the CPUs this process inherited; `steady`: prepare files before the timer where the tool can.
    fn open(tool: &str, dir: &Path, helper_cpus: Option<Vec<usize>>, steady: bool) -> Result<Self> {
        Ok(match tool {
            "tickertape" => {
                let mut config = WriterConfig::default();
                if let Some(cpus) = helper_cpus {
                    config = config.helper_cpus(HelperCpus::List(cpus));
                }
                Self::Tape(Writer::open(dir, config)?)
            }
            "plain-file" => Self::File(
                OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(dir.join("data"))?,
            ),
            "yaque" => Self::Yaque(
                yaque::SenderBuilder::new()
                    .segment_size(SEGMENT_SIZE)
                    .open(dir)?,
            ),
            "queue-file" => {
                // From its default size (8 KiB) the file doubles, with an fsync each time, inside the timer.
                let path = dir.join("data");
                let mut q = if steady {
                    queue_file::QueueFile::with_capacity(path, SEGMENT_SIZE)?
                } else {
                    queue_file::QueueFile::open(path)?
                };
                q.set_sync_writes(false);
                // Keep header publication per add: disabling this would make a
                // faster buffered mode with weaker process-crash semantics.
                Self::Queue(q)
            }
            _ => return Err("unknown tool".into()),
        })
    }
    fn send(&mut self, ts: u64, bytes: &[u8]) -> Result<()> {
        match self {
            Self::Tape(w) => {
                w.append(&NewRecord {
                    schema_id: 1,
                    venue_ts_ns: 0,
                    local_ts_ns: ts,
                    payload: &bytes[16..],
                })?;
            }
            Self::File(f) => f.write_all(bytes)?,
            Self::Yaque(w) => w.try_send(bytes).map_err(yaque::TrySendError::unwrap_io)?,
            Self::Queue(w) => w.add(bytes)?,
        }
        Ok(())
    }
    fn sync(&mut self) -> Result<Option<&'static str>> {
        Ok(match self {
            Self::Tape(w) => {
                w.checkpoint()?;
                Some("checkpoint")
            }
            Self::File(f) => {
                f.sync_all()?;
                Some("sync_all")
            }
            Self::Yaque(_) => None,
            Self::Queue(w) => {
                w.sync_all()?;
                Some("sync_all")
            }
        })
    }
}

impl Sink {
    /// The tool's closest equivalent of a periodic checkpoint for the sustained durability workload (SD), or an
    /// error if the tool exposes none.
    fn periodic_sync(&mut self) -> Result<&'static str> {
        Ok(match self {
            Self::Tape(w) => {
                w.checkpoint()?;
                "Writer::checkpoint (data + durable marker)"
            }
            Self::File(f) => {
                f.sync_data()?;
                "File::sync_data (fdatasync)"
            }
            Self::Queue(w) => {
                w.sync_all()?;
                "QueueFile::sync_all (fsync)"
            }
            Self::Yaque(_) => {
                return Err(
                    "yaque exposes no sync operation; periodic durability is unsupported".into(),
                );
            }
        })
    }
}

/// Writes `writer.done` (the final record count) atomically, so readers of an unpaced probe know when to stop.
fn publish_done(control: &Path, count: u64) -> Result<()> {
    fs::write(control.join("writer.done.tmp"), count.to_string())?;
    fs::rename(control.join("writer.done.tmp"), control.join("writer.done"))?;
    Ok(())
}

/// Reader stop condition. Paced and replay runs know their count; an unpaced probe learns it from `writer.done`.
struct Target {
    count: u64,
    probe: bool,
    known: bool,
    control: Option<PathBuf>,
}
impl Target {
    /// Called when no record was available: reads `writer.done` once it exists. Inlined into the reader's loop, as the
    /// compiler did when `run` had one instantiation; with two it would call it.
    #[inline(always)]
    fn idle(&mut self) -> Result<()> {
        let Some(control) = self.control.as_ref().filter(|_| self.probe && !self.known) else {
            return Ok(());
        };
        if let Ok(text) = fs::read_to_string(control.join("writer.done")) {
            self.count = text.trim().parse()?;
            self.known = true;
        }
        Ok(())
    }
}

/// `STAMP` is `--send-start on`. It is a const parameter so that `run::<false>`, the default, is the loops' previous
/// code with the option's blocks compiled out (`if STAMP` on a constant); `main_for` chooses the instantiation once.
/// `args` are the process's arguments without the program name.
fn run<const STAMP: bool>(tool: &str, args: Vec<String>) -> Result<()> {
    let role = args.first().ok_or("expected writer or reader")?;
    if role != "writer" && role != "reader" {
        return Err("expected writer or reader".into());
    }
    let opts = Options::parse(
        &args[1..],
        &[
            "--dir",
            "--control",
            "--count",
            "--payload",
            "--rate",
            "--warmup",
            "--reader",
            "--mode",
            "--duration-ms",
            "--sync-every-ms",
            "--cpu",
            "--helper-cpus",
            "--send-start",
        ],
        &[],
    )?;
    let dir = PathBuf::from(opts.required("--dir")?);
    let count = opts.number("--count", None)?;
    let payload: usize = opts.number("--payload", None)?.try_into()?;
    let rate = opts.number("--rate", Some(100_000))?;
    let warmup = opts.number("--warmup", Some(0))?;
    let mode = opts.required("--mode")?;
    // sustained_probe: unpaced sends with a concurrent reader until --duration-ms passes or --count is reached.
    let probe = mode == "sustained_probe";
    let live = mode == "visible_latency" || probe;
    let duration = Duration::from_millis(opts.number("--duration-ms", Some(0))?);
    if probe == duration.is_zero() {
        return Err("--duration-ms is required for, and only for, mode sustained_probe".into());
    }
    let sync_every = match opts.number("--sync-every-ms", Some(0))? {
        0 => None,
        ms if live => Some(Duration::from_millis(ms)),
        _ => return Err("--sync-every-ms needs a live mode".into()),
    };
    if count == 0 || rate == 0 || warmup >= count || !PAYLOADS.contains(&payload) {
        return Err("invalid count/rate/warmup/payload".into());
    }
    // --send-start on (`STAMP`): the writer also stores the send start (the clock read that ends its busy-wait, which
    // it takes anyway to count late sends) in the first 8 application bytes, and the reader records a second histogram
    // from it after the clock read of the first. Live modes only; off by default.
    if STAMP != (opts.optional("--send-start") == Some("on"))
        || !matches!(opts.optional("--send-start"), None | Some("on" | "off"))
        || (STAMP && !live)
    {
        return Err("--send-start takes on or off and needs a live mode".into());
    }
    let control = opts.path("--control");
    let helper_cpus = opts
        .optional("--helper-cpus")
        .map(parse_cpu_list)
        .transpose()?;
    // Pinning (README, Thread pinning): after open, so that threads the tool starts inherit the process's helper CPUs,
    // except where open is inside the timer (the cold append and W3's reader).
    let cpu = match opts.optional("--cpu") {
        Some(_) => Some(opts.number("--cpu", None)?),
        None => None,
    };
    // Tells the runner's Keeper about this thread: <role>.tid before it is pinned, <role>.measured after the read-back.
    let announce = |event: &str| -> Result<()> {
        if let (Some(_), Some(control)) = (cpu, control.as_deref()) {
            let tmp = control.join(format!("{role}.{event}.tmp"));
            fs::write(&tmp, thread_id()?.to_string())?;
            fs::rename(tmp, control.join(format!("{role}.{event}")))?;
        }
        Ok(())
    };
    let pin = || -> Result<()> {
        announce("tid")?;
        pin_or_fail(cpu)
    };
    // While the endpoint is open: closing it ends the tool's threads, and the read-back could not judge them.
    let read_back = || -> Result<Value> {
        let affinity = report()?;
        announce("measured")?;
        Ok(affinity)
    };
    let cold = mode == "append_cold";
    let open_is_timed = cold || (role == "reader" && !live);
    if open_is_timed {
        pin()?;
    }
    if role == "writer" {
        let priming = if mode == "append_steady" {
            PRIMING_RECORDS
        } else {
            0
        };
        let mut start = cold.then(Instant::now);
        let mut sink = Sink::open(tool, &dir, helper_cpus, priming > 0)?;
        if !open_is_timed {
            pin()?;
        }
        let mut bytes = vec![0x5a; payload + 16 + usize::from(tool == "plain-file") * 4];
        let offset = if tool == "plain-file" { 4 } else { 0 };
        if offset == 4 {
            bytes[..4].copy_from_slice(&((payload + 16) as u32).to_le_bytes());
        }
        let t0 = if live {
            wait_start(control.as_deref().ok_or("missing control")?, "writer")?
        } else {
            0
        };
        let mut late = 0;
        let mut max_late = 0;
        let mut sync_hist = Histogram::<u64>::new(3)?;
        let mut sync_operation = None;
        let mut sync_overruns = 0u64;
        let mut next_sync = sync_every.map(|every| Instant::now() + every);
        let mut sent = 0;
        let mut stopped_by = "count";
        if probe {
            while clock::realtime_ns() < t0 {
                std::hint::spin_loop();
            }
        }
        for i in 0..priming {
            bytes[offset..offset + 8].copy_from_slice(&i.to_le_bytes());
            sink.send(0, &bytes)?;
        }
        for i in 0..count {
            let intended = if probe {
                // An unpaced probe stores the actual send start, so reader latency is from the send.
                let now = clock::realtime_ns();
                if start.is_some_and(|s: Instant| s.elapsed() >= duration) {
                    stopped_by = "duration";
                    break;
                }
                if STAMP {
                    bytes[offset + 16..offset + 24].copy_from_slice(&now.to_le_bytes());
                }
                now
            } else if live {
                let intended = intended_ns(t0, i, rate)?;
                while clock::realtime_ns() < intended {
                    std::hint::spin_loop();
                }
                let delay = clock::realtime_ns() - intended;
                if delay > 1000 {
                    late += 1;
                }
                max_late = max_late.max(delay);
                if STAMP {
                    // The send start is the clock read of `delay`.
                    let at = intended + delay;
                    bytes[offset + 16..offset + 24].copy_from_slice(&at.to_le_bytes());
                }
                intended
            } else {
                0
            };
            start.get_or_insert_with(Instant::now);
            bytes[offset..offset + 8].copy_from_slice(&(priming + i).to_le_bytes());
            bytes[offset + 8..offset + 16].copy_from_slice(&intended.to_le_bytes());
            // Plain file's prefix is passed only to write(2), not other tools.
            sink.send(intended, &bytes)?;
            sent += 1;
            if let (Some(every), Some(next)) = (sync_every, next_sync.as_mut()) {
                let now = Instant::now();
                if now >= *next {
                    sync_operation = Some(sink.periodic_sync()?);
                    sync_hist.record(u64::try_from(now.elapsed().as_nanos())?)?;
                    // Fixed grid; grid points that passed during a slow sync are counted, not run late.
                    *next += every;
                    let after = Instant::now();
                    while *next <= after {
                        *next += every;
                        sync_overruns += 1;
                    }
                }
            }
        }
        let seconds = start.ok_or("no sends")?.elapsed().as_secs_f64();
        if live {
            publish_done(control.as_deref().ok_or("missing control")?, sent)?;
        }
        let sync_start = Instant::now();
        let sync = if live { None } else { sink.sync()? };
        let flush_seconds = sync.map(|_| sync_start.elapsed().as_secs_f64());
        let periodic = sync_every.map(|every| {
            json!({"operation": sync_operation, "interval_ms": every.as_millis() as u64,
                "count": sync_hist.len(), "grid_overruns": sync_overruns, "latency": histogram_json(&sync_hist)})
        });
        println!(
            "{}",
            json!({"count": sent, "planned_count": count, "seconds": seconds, "records_per_s": sent as f64 / seconds,
            "payload_mb_per_s": sent as f64 * payload as f64 / seconds / 1e6,
            "achieved_records_per_s": sent as f64 / seconds,
            "late_records": if probe { Value::Null } else { json!(late) },
            "max_late_us": if probe || !live { Value::Null } else { json!(max_late as f64 / 1e3) },
            "stopped_by": stopped_by, "periodic_sync": periodic,
            "flush_seconds": flush_seconds, "flush_operation": sync,
            "timer_starts": timer_starts(mode), "priming_records": priming, "affinity": read_back()?})
        );
        return Ok(());
    }
    let mut hist = Histogram::<u64>::new(3)?;
    // --send-start: the histogram from the send start; it does not exist without the option.
    let mut from_send = if STAMP {
        Some(Histogram::<u64>::new(3)?)
    } else {
        None
    };
    let start = Instant::now(); // Includes fresh reader opening for W3.
    // A probe's count is only a disk cap: its deadline follows the duration instead.
    let deadline = start
        + Duration::from_secs(120)
        + if probe {
            duration
        } else {
            Duration::from_secs(count / rate)
        };
    let mut expected = 0;
    let mut target = Target {
        count,
        probe,
        known: !probe,
        control: control.clone(),
    };
    let mut observe = |seq: u64, ts: u64, len: usize| -> Result<()> {
        if seq != expected || len != payload {
            return Err("record sequence or length mismatch".into());
        }
        if live && expected >= warmup {
            if STAMP {
                let now = clock::realtime_ns();
                hist.record(now.checked_sub(ts).ok_or("wall clock moved backwards")?)?;
                LAST_READ.store(now, Ordering::Relaxed);
            } else {
                hist.record(
                    clock::realtime_ns()
                        .checked_sub(ts)
                        .ok_or("wall clock moved backwards")?,
                )?;
            }
        }
        expected += 1;
        Ok(())
    };
    // After `observe`, with the record's application bytes: from the send start to the clock read `observe` took.
    let mut observe_sent = |application: &[u8]| -> Result<()> {
        let Some(from_send) = from_send.as_mut() else {
            return Ok(());
        };
        let now = LAST_READ.load(Ordering::Relaxed);
        LAST_READ.store(0, Ordering::Relaxed);
        if now != 0 {
            from_send.record(
                now.checked_sub(send_start(application)?)
                    .ok_or("wall clock moved backwards")?,
            )?;
        }
        Ok(())
    };
    let ready = || -> Result<()> {
        if live {
            pin()?;
            wait_start(control.as_deref().ok_or("missing control")?, "reader")?;
        }
        Ok(())
    };
    let idle = |target: &mut Target| -> Result<()> {
        if Instant::now() > deadline {
            return Err("reader timed out".into());
        }
        target.idle()?;
        std::hint::spin_loop();
        Ok(())
    };
    // `received` mirrors `expected` (which `observe` borrows) for the loop conditions.
    let mut received = 0u64;
    let (seconds, affinity) = match tool {
        "tickertape" => {
            let cfg = ReaderConfig::default();
            let mut r = if opts.required("--reader")? == "mapped" {
                // SAFETY: this run owns a fresh journal, modified only by Writer.
                unsafe { Reader::open_mapped(&dir, cfg)? }
            } else {
                Reader::open(&dir, cfg)?
            };
            ready()?;
            while received < target.count {
                if let Some(record) = r.try_next()? {
                    std::hint::black_box(record.payload);
                    observe(record.seq, record.local_ts_ns, record.payload.len())?;
                    if STAMP {
                        observe_sent(record.payload)?;
                    }
                    received += 1;
                } else {
                    idle(&mut target)?;
                }
            }
            (start.elapsed().as_secs_f64(), read_back()?)
        }
        "plain-file" => {
            let mut r = FileReader {
                file: File::open(dir.join("data"))?,
                frame: vec![0; payload + 20],
                filled: 0,
            };
            ready()?;
            while received < target.count {
                if let Some(bytes) = r.poll()? {
                    observe(
                        received,
                        validate(bytes, received, payload)?,
                        bytes.len() - 16,
                    )?;
                    if STAMP {
                        observe_sent(&bytes[16..])?;
                    }
                    received += 1;
                } else {
                    idle(&mut target)?;
                }
            }
            (start.elapsed().as_secs_f64(), read_back()?)
        }
        "yaque" if live => {
            let mut r = yaque::ReceiverBuilder::new()
                .save_every_nth(None)
                .save_every(None)
                .open(&dir)?;
            ready()?;
            while received < target.count {
                match r.try_recv() {
                    Ok(record) => {
                        observe(
                            received,
                            validate(&record, received, payload)?,
                            record.len() - 16,
                        )?;
                        if STAMP {
                            observe_sent(&record[16..])?;
                        }
                        record.commit()?;
                        received += 1;
                    }
                    Err(yaque::TryRecvError::QueueEmpty) => idle(&mut target)?,
                    Err(e) => return Err(e.unwrap_io().into()),
                }
            }
            (start.elapsed().as_secs_f64(), read_back()?)
        }
        "yaque" => {
            let r = yaque::QueueIter::open(&dir)?;
            let mut iter = r.take(count as usize).enumerate();
            for (i, record) in iter.by_ref() {
                let record = record?;
                observe(
                    i as u64,
                    validate(&record, i as u64, payload)?,
                    record.len() - 16,
                )?;
                received += 1;
            }
            (start.elapsed().as_secs_f64(), read_back()?)
        }
        "queue-file" => {
            if live {
                return Err("queue-file does not support concurrent processes".into());
            }
            let r = queue_file::QueueFile::open(dir.join("data"))?;
            let mut iter = r.iter();
            for i in 0..count {
                let bytes = iter.borrowed_next().ok_or("truncated queue")?;
                observe(i, validate(bytes, i, payload)?, bytes.len() - 16)?;
                received += 1;
            }
            (start.elapsed().as_secs_f64(), read_back()?)
        }
        _ => return Err("unknown tool".into()),
    };
    if received != target.count || expected != target.count {
        return Err("incomplete replay".into());
    }
    let read = target.count;
    let mut result = if probe {
        json!({"count": read, "latency_from": "send start", "latency": histogram_json(&hist)})
    } else if live {
        let mut result = live_histogram(&hist);
        if let Some(from_send) = &from_send {
            result["from_send_start"] = live_histogram(from_send);
        }
        result
    } else {
        json!({"count": count, "seconds": seconds,
        "records_per_s": count as f64 / seconds, "payload_mb_per_s": count as f64 * payload as f64 / seconds / 1e6})
    };
    result["affinity"] = affinity;
    println!("{result}");
    Ok(())
}

/// Where the writer's timer starts, for the result.
fn timer_starts(mode: &str) -> &'static str {
    match mode {
        "append_cold" => "before open, in an empty directory",
        "append_steady" => "at the first record after open and the priming records",
        _ => "at the first record after open",
    }
}

pub fn main_for(tool: &str) {
    // The instantiation is chosen once, before anything runs; `run` checks the option itself.
    let args: Vec<_> = std::env::args().skip(1).collect();
    let stamp = args
        .windows(2)
        .any(|w| w[0] == "--send-start" && w[1] == "on");
    let result = if stamp {
        run::<true>(tool, args)
    } else {
        run::<false>(tool, args)
    };
    if let Err(e) = result {
        eprintln!("{tool}: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_follower_retains_partial_header_and_payload_at_eof() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("data");
        let mut writer = File::create(&path).unwrap();
        let mut reader = FileReader {
            file: File::open(&path).unwrap(),
            frame: vec![0; 84],
            filled: 0,
        };
        let mut frame = vec![0x5a; 84];
        frame[..4].copy_from_slice(&80_u32.to_le_bytes());
        frame[4..12].copy_from_slice(&0_u64.to_le_bytes());
        frame[12..20].copy_from_slice(&123_u64.to_le_bytes());
        assert!(reader.poll().unwrap().is_none());
        for chunk in frame.chunks(3) {
            writer.write_all(chunk).unwrap();
            if reader.filled + chunk.len() < 84 {
                assert!(reader.poll().unwrap().is_none());
            }
        }
        let bytes = reader.poll().unwrap().unwrap();
        assert_eq!(validate(bytes, 0, 64).unwrap(), 123);
        assert!(reader.poll().unwrap().is_none());
        writer.write_all(&frame).unwrap();
        assert!(reader.poll().unwrap().is_some());
    }

    #[test]
    fn ladder_has_every_percentile_in_order_and_the_maximum() {
        let mut hist = Histogram::<u64>::new(3).unwrap();
        for v in 1..=100_000u64 {
            hist.record(v).unwrap();
        }
        let stats = live_histogram(&hist);
        let ladder = stats["percentiles_us"].as_object().unwrap();
        let names: Vec<_> = ladder.keys().cloned().collect();
        let mut expected: Vec<String> = LADDER.iter().map(|(n, _)| n.to_string()).collect();
        expected.push("max".into());
        assert_eq!(names, expected);
        let values: Vec<f64> = ladder.values().map(|v| v.as_f64().unwrap()).collect();
        assert!(values.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(ladder["50"], stats["p50_us"]);
        assert_eq!(ladder["99.99"], stats["p99_99_us"]);
        assert_eq!(ladder["max"], stats["max_us"]);
        // 3 significant digits: 99,999 ns is in the bucket of 99.9 µs.
        assert!((ladder["99.999"].as_f64().unwrap() - 100.0).abs() < 0.2);
        let empty = live_histogram(&Histogram::<u64>::new(3).unwrap());
        assert!(empty["percentiles_us"]["99.9"].is_null());
    }

    #[test]
    fn send_start_is_the_first_eight_application_bytes() {
        let mut application = vec![0x5a; 256];
        application[..8].copy_from_slice(&1_234_567_u64.to_le_bytes());
        assert_eq!(send_start(&application).unwrap(), 1_234_567);
        assert!(send_start(&application[..7]).is_err());
    }

    #[test]
    fn rejects_wrong_sequence_and_frame_length() {
        assert!(validate(&[0; 80], 1, 64).is_err());
        assert!(validate(&[0; 16], 0, 64).is_err());
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("data");
        fs::write(&path, [0; 84]).unwrap();
        let mut reader = FileReader {
            file: File::open(path).unwrap(),
            frame: vec![0; 84],
            filled: 0,
        };
        assert!(reader.poll().is_err());
    }
}
