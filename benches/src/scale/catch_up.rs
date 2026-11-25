//! `tt-scale catch-up`: a reader that starts far behind a writer appending at a steady rate. The writer runs in this
//! process, the reader in a child; the writer's latency is recorded apart for the time before the reader starts, while
//! it catches up, and after.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use serde_json::{Value, json};
use tickertape::{NewRecord, Reader, Writer, WriterConfig, clock};

use super::{Journal, ReaderKind, drop_caches, histogram, payload_size, timed};
use crate::affinity::pin_or_fail;
use crate::sustained::{ReaderSpec, WaitMode, record_bytes, total_ram_bytes};
use crate::{Result, elapsed_ns, histogram_json, intended_ns};

/// One run.
#[derive(Debug, Clone)]
pub struct Config {
    /// The journal (checkpointed at its end, as `build` or an earlier run leaves it). The run appends to it.
    pub dir: PathBuf,
    /// The reader.
    pub reader: ReaderKind,
    /// How it waits once it has caught up.
    pub wait: WaitMode,
    /// Records per second the writer appends.
    pub rate: u64,
    /// How many records before the head the reader starts, as of the writer's start.
    pub backlog: u64,
    /// Time before anything is recorded.
    pub warmup: Duration,
    /// Time before the reader starts.
    pub baseline: Duration,
    /// Time after it has caught up.
    pub live: Duration,
    /// The run stops if catching up takes longer than this.
    pub limit: Duration,
    /// Background checkpoint interval; zero for none.
    pub checkpoint_every: Duration,
    /// CPUs to pin the writer and the reader to.
    pub cpus: Option<(u64, u64)>,
    /// Drop the page cache before the run.
    pub cold: bool,
}

const WARMUP: u8 = 0;
const BASELINE: u8 = 1;
const CATCH_UP: u8 = 2;
const LIVE: u8 = 3;
const FAILED: u8 = 4;
const PHASES: [&str; 4] = ["warmup", "baseline", "catch_up", "live"];

/// What the writer saw during one phase.
struct Phase {
    records: u64,
    late: u64,
    /// How long `append` took.
    append: Histogram<u64>,
    /// From the intended time to the end of `append`: a delayed schedule counts.
    intended: Histogram<u64>,
    checkpoints: u64,
}

impl Phase {
    fn new() -> Result<Phase> {
        Ok(Phase {
            records: 0,
            late: 0,
            append: histogram()?,
            intended: histogram()?,
            checkpoints: 0,
        })
    }

    fn json(&self) -> Value {
        json!({"records": self.records, "late_records": self.late, "checkpoints_requested": self.checkpoints,
            "append_latency": histogram_json(&self.append), "from_intended_latency": histogram_json(&self.intended)})
    }
}

/// Kills the reader if the run fails before it has ended by itself.
struct Reaper(Child);

impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Runs the writer, starting the reader with `start_reader` (a child process that runs [`reader`]).
pub fn run(config: &Config, start_reader: impl FnOnce(u64) -> Result<Command>) -> Result<Value> {
    if config.rate == 0 {
        return Err("rate must be positive".into());
    }
    let journal = Journal::describe(&config.dir, None)?;
    let payload = vec![0x5a; payload_size(&config.dir)?];
    let start_seq = journal
        .next_seq
        .saturating_sub(config.backlog)
        .max(journal.first_seq);
    let dropped = config.cold && drop_caches();
    let phase = Arc::new(AtomicU8::new(WARMUP));
    // Built before this thread is pinned: children and threads inherit the mask.
    let mut command = start_reader(start_seq)?;
    let (go, started) = std::sync::mpsc::channel::<()>();
    let seen = Arc::clone(&phase);
    let reader = std::thread::spawn(
        move || -> std::result::Result<(Reaper, Option<Value>), String> {
            // The spawning thread waits here unpinned until the baseline is over.
            started.recv().map_err(|e| e.to_string())?;
            let mut child = Reaper(
                command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .map_err(|e| e.to_string())?,
            );
            let stdout = child.0.stdout.take().ok_or("no stdout from the reader")?;
            for line in BufReader::new(stdout).lines() {
                let line = line.map_err(|e| e.to_string())?;
                let value: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
                match value["event"].as_str() {
                    Some("start") => seen.store(CATCH_UP, Ordering::Relaxed),
                    Some("caught-up") => {
                        seen.store(LIVE, Ordering::Relaxed);
                        return Ok((child, Some(value)));
                    }
                    _ => {
                        seen.store(FAILED, Ordering::Relaxed);
                        return Ok((child, Some(value)));
                    }
                }
            }
            seen.store(FAILED, Ordering::Relaxed);
            Ok((child, None))
        },
    );
    pin_or_fail(config.cpus.map(|c| c.0))?;
    let mut writer = Writer::open(
        &config.dir,
        WriterConfig::default().segment_size(journal.segment_size),
    )?;
    let mut phases = [Phase::new()?, Phase::new()?, Phase::new()?, Phase::new()?];
    let t0 = clock::realtime_ns() + 10_000_000;
    let baseline_from = t0 + u64::try_from(config.warmup.as_nanos())?;
    let reader_from = baseline_from + u64::try_from(config.baseline.as_nanos())?;
    let limit = u64::try_from(config.limit.as_nanos())?;
    let live = u64::try_from(config.live.as_nanos())?;
    let every = u64::try_from(config.checkpoint_every.as_nanos())?;
    let mut next_checkpoint = t0 + every;
    let mut go = Some(go);
    let mut reader_started = false;
    let mut live_from = None;
    let mut index = 0u64;
    let outcome = loop {
        let intended = intended_ns(t0, index, config.rate)?;
        let mut now = clock::realtime_ns();
        let late = now > intended + 1_000;
        while now < intended {
            std::hint::spin_loop();
            now = clock::realtime_ns();
        }
        let current = match phase.load(Ordering::Relaxed) {
            FAILED => break "reader failed",
            LIVE if now - *live_from.get_or_insert(now) >= live => break "done",
            WARMUP if now >= baseline_from => {
                phase.store(BASELINE, Ordering::Relaxed);
                BASELINE
            }
            BASELINE if now >= reader_from && !reader_started => {
                reader_started = true;
                if let Some(go) = go.take() {
                    let _ = go.send(());
                }
                BASELINE
            }
            CATCH_UP | BASELINE if reader_started && now - reader_from > limit => {
                break "reader did not catch up";
            }
            other => other,
        };
        let start = Instant::now();
        writer.append(&NewRecord {
            schema_id: 1,
            venue_ts_ns: index,
            local_ts_ns: intended,
            payload: &payload,
        })?;
        let took = elapsed_ns(start.elapsed())?;
        let stats = &mut phases[usize::from(current)];
        stats.records += 1;
        stats.late += u64::from(late);
        stats.append.saturating_record(took);
        stats
            .intended
            .saturating_record(clock::realtime_ns().saturating_sub(intended).max(1));
        if every != 0 && intended >= next_checkpoint {
            next_checkpoint += every;
            stats.checkpoints += 1;
            writer.request_checkpoint()?;
        }
        index += 1;
    };
    let end_seq = writer.next_seq();
    drop(go);
    writer.checkpoint()?;
    writer.close()?;
    if outcome == "reader did not catch up" {
        // The reader stops when its standard input closes with this process.
        return Err(outcome.into());
    }
    let mut result = json!({"outcome": outcome, "journal": journal.json(), "payload": payload.len(),
        "reader": config.reader.name(), "wait": config.wait.name(), "rate": config.rate,
        "backlog_records": journal.next_seq - start_seq,
        "backlog_bytes": (journal.next_seq - start_seq) * record_bytes(payload.len())?,
        "checkpoint_every_s": config.checkpoint_every.as_secs_f64(),
        "cpus": config.cpus.map(|c| [c.0, c.1]), "cold_requested": config.cold, "cache_dropped": dropped,
        "ram_bytes": total_ram_bytes(), "records_appended": index, "end_seq": end_seq});
    for (name, phase) in PHASES.iter().zip(&phases) {
        result["writer"][*name] = phase.json();
    }
    let (mut child, event) = reader
        .join()
        .map_err(|_| "the reader's thread panicked")?
        .map_err(|e| format!("reader: {e}"))?;
    result["caught_up"] = event.unwrap_or(Value::Null);
    if outcome == "done" {
        // The end of standard input, with the last sequence number, tells the reader to stop there.
        if let Some(mut stdin) = child.0.stdin.take() {
            write!(stdin, "{end_seq}")?;
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        while child.0.try_wait()?.is_none() {
            if Instant::now() > deadline {
                return Err("the reader did not stop".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let summary = std::fs::read_to_string(summary_path(&config.dir))?;
        std::fs::remove_file(summary_path(&config.dir))?;
        result["live"] = serde_json::from_str(&summary)?;
    }
    Ok(result)
}

/// Where the reader leaves what it saw after it caught up.
fn summary_path(dir: &std::path::Path) -> PathBuf {
    dir.with_extension("catch-up.json")
}

/// The reader process: reads from `from` to the head, says so, then follows the writer until standard input ends with
/// the sequence number to stop at. Every record's sequence number is checked.
pub fn reader(
    dir: &std::path::Path,
    kind: ReaderKind,
    wait: WaitMode,
    from: u64,
    cpu: Option<u64>,
) -> Result<()> {
    let spec = ReaderSpec {
        mapped: kind == ReaderKind::Mapped,
        durable_only: false,
        wait,
        delay: None,
        stall: None,
    };
    let stop = Arc::new(AtomicBool::new(false));
    let ended = Arc::clone(&stop);
    let end = std::thread::spawn(move || -> Option<u64> {
        let mut text = String::new();
        let read = std::io::stdin().read_to_string(&mut text);
        ended.store(true, Ordering::Release);
        read.ok()?;
        text.trim().parse().ok()
    });
    pin_or_fail(cpu)?;
    let emit = |value: Value| -> Result<()> {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{value}")?;
        out.flush()?;
        Ok(())
    };
    let mut reader = kind.open(dir, spec.reader_config())?;
    reader.seek(from)?;
    emit(json!({"event": "start", "from": from}))?;
    let mut expected = from;
    let mut payload_bytes = 0u64;
    let check = |found: u64, expected: &mut u64| -> Result<()> {
        if found != *expected {
            return Err(format!("the reader returned {found}, not {expected}").into());
        }
        *expected += 1;
        Ok(())
    };
    let mut caught_up = timed(|| {
        while let Some(record) = reader.try_next()? {
            payload_bytes += record.payload.len() as u64;
            check(record.seq, &mut expected)?;
        }
        Ok(json!({"records": expected - from}))
    })?;
    caught_up["payload_bytes"] = json!(payload_bytes);
    let wall = caught_up["wall_s"].as_f64().unwrap_or(0.0);
    if wall > 0.0 {
        caught_up["records_per_s"] = json!((expected - from) as f64 / wall);
        caught_up["payload_bytes_per_s"] = json!(payload_bytes as f64 / wall);
    }
    caught_up["event"] = json!(if caught_up.get("error").is_some() {
        "failed"
    } else {
        "caught-up"
    });
    emit(caught_up)?;
    let mut latency = histogram()?;
    let live_from = expected;
    let mut live = timed(|| {
        let mut until = None;
        while until.is_none_or(|end| expected < end) {
            if let Some((seq, intended)) = next(&mut reader)? {
                latency.saturating_record(clock::realtime_ns().saturating_sub(intended).max(1));
                check(seq, &mut expected)?;
            }
            if until.is_none() && stop.load(Ordering::Acquire) {
                // Standard input ended: its number is where to stop. Without one the writer is gone.
                until = Some(0);
            }
        }
        Ok(json!({}))
    })?;
    // Read what is left up to the writer's end, if it gave one.
    let end = end.join().map_err(|_| "the input thread panicked")?;
    while end.is_some_and(|end| expected < end) {
        let (seq, _) = next(&mut reader)?.ok_or("the reader lost records at the end")?;
        check(seq, &mut expected)?;
    }
    live["records"] = json!(expected - live_from);
    live["visible_latency"] = histogram_json(&latency);
    live["end_seq"] = json!(expected);
    std::fs::write(summary_path(dir), live.to_string())?;
    Ok(())
}

fn next(reader: &mut Reader) -> Result<Option<(u64, u64)>> {
    Ok(reader
        .next_timeout(Some(Duration::from_millis(100)))?
        .map(|r| (r.seq, r.local_ts_ns)))
}
