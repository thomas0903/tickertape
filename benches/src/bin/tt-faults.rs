//! Page faults on the writer's thread, append by append (ADR 0058): which appends take a fault, how long they take
//! against the appends that do not, and where in the segment they are. Linux only (it reads the thread's fault counts
//! with `getrusage(RUSAGE_THREAD)`).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use serde_json::json;
use tickertape::{
    Backoff, HelperCpus, NewRecord, Reader, ReaderConfig, Writer, WriterConfig, clock,
};
use tickertape_bench::sustained::{parse_cpu_list, pin_current_thread};
use tickertape_bench::{Options, Result, fresh_directory, histogram_json};

const HELP: &str = "tt-faults: page faults on the writer's thread, append by append (Linux)

Usage: tt-faults --dir <DIR> --rate <records/s> --payload <bytes> --seconds <S> [--warmup-seconds <S>]
                 [--checkpoint-ms <MS>] [--no-prefault] [--no-early-writeback] [--spare-segment]
                 [--segment-size <bytes>] [--writer-cpu <N>] [--reader-cpu <N>] [--helper-cpus <LIST>]
                 [--touch none|idle] [--log <FILE>] [--slow-us <US>]

Paced appends on the main thread (named tt-faults, for tracing), scheduled on the wall clock as tt-latency does; the
latency of an append runs from its scheduled send to the return of Writer::append. The thread's minor and major fault
counts are read with getrusage(RUSAGE_THREAD) before the wait for the send and after the append, outside the timed
part unless the append is late. --checkpoint-ms requests a background checkpoint (Writer::request_checkpoint) at that
interval. --reader-cpu adds a zero-copy reader on a thread of its own that busy-polls and measures the visible
latency. --touch idle calls Writer::exp_touch once in each wait (needs --cfg tickertape_pf_exp). --log writes one line
per measured append that took a fault or more than --slow-us (default 20): its scheduled wall time (ns), latency
(ns), minor and major faults, segment base sequence and offset, and ms since the last checkpoint request. Prints JSON.";

fn hist() -> Result<Histogram<u64>> {
    Ok(Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3)?)
}

#[cfg(target_os = "linux")]
fn faults() -> Result<(u64, u64)> {
    let usage = nix::sys::resource::getrusage(nix::sys::resource::UsageWho::RUSAGE_THREAD)?;
    Ok((
        usage.minor_page_faults().try_into()?,
        usage.major_page_faults().try_into()?,
    ))
}

#[cfg(not(target_os = "linux"))]
fn faults() -> Result<(u64, u64)> {
    Err("tt-faults needs Linux (getrusage with RUSAGE_THREAD)".into())
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP}");
        return;
    }
    if let Err(error) = run(&args) {
        eprintln!("tt-faults: {error}");
        std::process::exit(1);
    }
}

#[allow(clippy::too_many_lines)]
fn run(args: &[String]) -> Result<()> {
    let options = Options::parse(
        args,
        &[
            "--dir",
            "--rate",
            "--payload",
            "--seconds",
            "--warmup-seconds",
            "--checkpoint-ms",
            "--segment-size",
            "--writer-cpu",
            "--reader-cpu",
            "--helper-cpus",
            "--touch",
            "--log",
            "--slow-us",
        ],
        &["--no-prefault", "--no-early-writeback", "--spare-segment"],
    )?;
    let dir = std::path::PathBuf::from(options.required("--dir")?);
    let rate = options.number("--rate", None)?;
    let payload = vec![0x5a_u8; options.number("--payload", None)?.try_into()?];
    let seconds = options.number("--seconds", None)?;
    let warmup = options.number("--warmup-seconds", Some(2))?;
    let checkpoint_ms = options.number("--checkpoint-ms", Some(0))?;
    let slow_ns = options.number("--slow-us", Some(20))? * 1000;
    let touch = options.optional("--touch").unwrap_or("none").to_owned();
    if !["none", "idle"].contains(&touch.as_str()) {
        return Err("--touch must be none or idle".into());
    }
    #[cfg(not(tickertape_pf_exp))]
    if touch != "none" {
        return Err("--touch idle needs a build with --cfg tickertape_pf_exp".into());
    }
    let cpu = |key: &str| -> Result<Option<usize>> {
        options
            .optional(key)
            .map(|_| options.number(key, None).map(|n| n as usize))
            .transpose()
    };
    let writer_cpu = cpu("--writer-cpu")?;
    let reader_cpu = cpu("--reader-cpu")?;
    fresh_directory(&dir)?;
    let mut config = WriterConfig::default()
        .segment_size(options.number("--segment-size", Some(WriterConfig::DEFAULT_SEGMENT_SIZE))?)
        .prefault(!options.flag("--no-prefault"))
        .early_writeback(!options.flag("--no-early-writeback"))
        .spare_segment(options.flag("--spare-segment"));
    if let Some(list) = options.optional("--helper-cpus") {
        config = config.helper_cpus(HelperCpus::List(
            parse_cpu_list(list).map_err(|e| format!("--helper-cpus: {e}"))?,
        ));
    }
    let mut writer = Writer::open(&dir, config)?;
    if let Some(cpu) = writer_cpu {
        pin_current_thread(cpu)?;
    }

    let stop = Arc::new(AtomicBool::new(false));
    let reader = match reader_cpu {
        None => None,
        Some(cpu) => {
            let stop = Arc::clone(&stop);
            let dir = dir.clone();
            let measure_from = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
            let from = Arc::clone(&measure_from);
            let handle = std::thread::Builder::new().name("tt-reader".into()).spawn(
                move || -> std::result::Result<Histogram<u64>, String> {
                    let read = || -> Result<Histogram<u64>> {
                        pin_current_thread(cpu)?;
                        let config = ReaderConfig::default()
                            .backoff(Backoff::busy_spin())
                            .notify(false);
                        // SAFETY: only this process's tickertape writer modifies the journal, in a fresh directory.
                        let mut reader = unsafe { Reader::open_mapped(&dir, config)? };
                        let mut h = hist()?;
                        while !stop.load(Ordering::Relaxed) {
                            if let Some(record) =
                                reader.next_timeout(Some(Duration::from_millis(50)))?
                            {
                                let now = clock::realtime_ns();
                                if record.local_ts_ns >= from.load(Ordering::Relaxed) {
                                    h.record(now.saturating_sub(record.local_ts_ns).max(1))?;
                                }
                            }
                        }
                        Ok(h)
                    };
                    read().map_err(|e| e.to_string())
                },
            )?;
            Some((handle, measure_from))
        }
    };

    let mut log = match options.optional("--log") {
        Some(path) => Some(BufWriter::with_capacity(1 << 20, File::create(path)?)),
        None => None,
    };
    let (mut all, mut faulted, mut clean) = (hist()?, hist()?, hist()?);
    let total = rate * (warmup + seconds);
    let measured_from_index = rate * warmup;
    std::thread::sleep(Duration::from_millis(200));
    let t0 = clock::realtime_ns() + 50_000_000;
    let measure_from_ns = tickertape_bench::intended_ns(t0, measured_from_index, rate)?;
    if let Some((_, from)) = &reader {
        from.store(measure_from_ns, Ordering::Relaxed);
    }
    let checkpoint_every = checkpoint_ms * 1_000_000;
    let mut next_checkpoint = if checkpoint_every > 0 {
        t0 + checkpoint_every
    } else {
        u64::MAX
    };
    let mut last_checkpoint = t0;
    let (mut late, mut checkpoints) = (0u64, 0u64);
    let (mut minor_total, mut major_total, mut faulted_appends) = (0u64, 0u64, 0u64);
    #[cfg_attr(not(tickertape_pf_exp), allow(unused_mut))]
    let (mut idle_minor, mut idle_calls) = (0u64, 0u64);
    let mut by_class = [0u64; 3]; // faulted appends in the first 256 KiB of a segment, within 5 ms of a checkpoint request, elsewhere
    let mut after = faults()?;
    let started = Instant::now();
    let record_bytes = tickertape::format::align8(u64::from(
        tickertape::format::frame_len(payload.len()).ok_or("payload too large")?,
    ));
    for i in 0..total {
        let intended = tickertape_bench::intended_ns(t0, i, rate)?;
        let measured = i >= measured_from_index;
        #[cfg_attr(not(tickertape_pf_exp), allow(unused_mut))]
        let mut before = after;
        let mut now = clock::realtime_ns();
        if now < intended {
            #[cfg(tickertape_pf_exp)]
            if touch == "idle" {
                writer.exp_touch();
                idle_calls += 1;
                let counted = faults()?;
                if measured {
                    idle_minor += counted.0 - before.0;
                }
                before = counted;
            }
            while now < intended {
                std::hint::spin_loop();
                now = clock::realtime_ns();
            }
        }
        if measured && now - intended > 1000 {
            late += 1;
        }
        writer.append(&NewRecord {
            schema_id: 1,
            venue_ts_ns: 0,
            local_ts_ns: intended,
            payload: &payload,
        })?;
        let end = clock::realtime_ns();
        after = faults()?;
        let latency = end.saturating_sub(intended).max(1);
        let (minor, major) = (after.0 - before.0, after.1 - before.1);
        if measured {
            all.record(latency)?;
            if minor + major > 0 {
                faulted_appends += 1;
                minor_total += minor;
                major_total += major;
                faulted.record(latency)?;
            } else {
                clean.record(latency)?;
            }
            if minor + major > 0 || latency >= slow_ns {
                let head = writer.metrics().head;
                let since_ckpt_ms = (intended.saturating_sub(last_checkpoint)) as f64 / 1e6;
                if minor + major > 0 {
                    by_class[if head.offset < (256 << 10) + record_bytes {
                        0
                    } else if checkpoint_every > 0 && since_ckpt_ms < 5.0 {
                        1
                    } else {
                        2
                    }] += 1;
                }
                if let Some(log) = &mut log {
                    writeln!(
                        log,
                        "{intended} {latency} {minor} {major} {} {} {since_ckpt_ms:.3}",
                        head.segment_base_seq, head.offset
                    )?;
                }
            }
        }
        if end >= next_checkpoint {
            writer.request_checkpoint()?;
            last_checkpoint = end;
            checkpoints += 1;
            while next_checkpoint <= end {
                next_checkpoint += checkpoint_every;
            }
            after = faults()?;
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    let head = writer.metrics().head;
    stop.store(true, Ordering::Relaxed);
    let reader_hist = match reader {
        Some((handle, _)) => Some(handle.join().map_err(|_| "reader panicked")??),
        None => None,
    };
    if let Some(log) = &mut log {
        log.flush()?;
    }
    writer.close()?;
    let measured = rate * seconds;
    let mib = (measured * record_bytes) as f64 / f64::from(1 << 20);
    let result = json!({
        "rate": rate, "payload": payload.len(), "seconds": seconds, "warmup_seconds": warmup,
        "checkpoint_ms": checkpoint_ms, "touch": touch,
        "elapsed_s": elapsed, "appends_measured": measured, "record_bytes": record_bytes, "mib_measured": mib,
        "final_head": {"segment_base_seq": head.segment_base_seq, "offset": head.offset},
        "checkpoints_requested": checkpoints,
        "late_sends": late,
        "faults": {
            "minor": minor_total, "major": major_total, "faulted_appends": faulted_appends,
            "per_append": (minor_total + major_total) as f64 / measured as f64,
            "per_mib": (minor_total + major_total) as f64 / mib,
            "faulted_appends_segment_start": by_class[0],
            "faulted_appends_after_checkpoint_5ms": by_class[1],
            "faulted_appends_elsewhere": by_class[2],
            "idle_touch_calls": idle_calls, "idle_touch_minor": idle_minor,
        },
        "append_latency": histogram_json(&all),
        "append_latency_faulted": histogram_json(&faulted),
        "append_latency_clean": histogram_json(&clean),
        "reader_latency": reader_hist.as_ref().map(histogram_json),
        "env": std::env::vars().filter(|(k, _)| k.starts_with("TT_PF_")).collect::<std::collections::BTreeMap<_, _>>(),
    });
    println!("{result}");
    Ok(())
}
