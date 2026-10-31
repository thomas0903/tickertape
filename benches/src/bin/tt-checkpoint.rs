//! Checkpoint latency and fixed-size append latency, separating roll appends.
//! Fresh journals permit format-based roll classification outside the timed append.

use std::path::PathBuf;
use std::time::Instant;

use hdrhistogram::Histogram;
use serde_json::json;
use tickertape::{NewRecord, Writer, WriterConfig};
use tickertape_bench::{
    Options, Result, elapsed_ns, fresh_directory, histogram_json, print_histogram, segment_capacity,
};

const HELP: &str = "tt-checkpoint: checkpoint and segment-roll append latency

Usage: tt-checkpoint --dir <DIR> --payload <bytes> --batch <records-per-checkpoint>
                     --checkpoints <N> [--segment-size <bytes>] [--json]
       tt-checkpoint --help

--dir must be new or empty; data is retained. Batch and checkpoints must be positive.
--segment-size defaults to 134217728 (128 MiB); use 65536 to exercise frequent rolls.
Segment sizes must be multiples of 65536 and each payload must fit in one segment.

Appends batch records, then times checkpoint(), N times. Also times one final
checkpoint with nothing new. Each append is timed separately; fixed record size and
sequence/segment capacity identify appends that create a new segment. Initial
segment creation is excluded; roll append latency includes segment creation.
Reports independent HdrHistogram distributions (3 significant figures, ns) for
dirty checkpoints, the empty checkpoint, roll appends and other appends, in us.
Empty categories have null statistics. Overall records/s includes appends, timing
instrumentation and dirty checkpoints; excludes writer open and the empty checkpoint.
Uses a monotonic clock for durations. Results depend on storage and filesystem;
this does not emulate power failure. No core pinning, root or network required.
CI runs are functional smoke checks, never recorded benchmark results.";

fn run(args: &[String]) -> Result<()> {
    let options = Options::parse(
        args,
        &[
            "--dir",
            "--payload",
            "--batch",
            "--checkpoints",
            "--segment-size",
        ],
        &["--json"],
    )?;
    let dir = PathBuf::from(options.required("--dir")?);
    let payload_size: usize = options.number("--payload", None)?.try_into()?;
    let batch = options.number("--batch", None)?;
    let checkpoints = options.number("--checkpoints", None)?;
    if batch == 0 || checkpoints == 0 {
        return Err("batch and checkpoints must be positive".into());
    }
    let count = batch
        .checked_mul(checkpoints)
        .filter(|v| *v < u64::MAX)
        .ok_or("record count overflow")?;
    let segment_size =
        options.number("--segment-size", Some(WriterConfig::DEFAULT_SEGMENT_SIZE))?;
    let capacity = segment_capacity(segment_size, payload_size)?;
    fresh_directory(&dir)?;
    let payload = vec![0x5a; payload_size];
    let record = NewRecord {
        schema_id: 1,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload: &payload,
    };
    let mut writer = Writer::open(&dir, WriterConfig::default().segment_size(segment_size))?;
    let mut dirty = Histogram::<u64>::new(3)?;
    let mut empty = Histogram::<u64>::new(3)?;
    let mut roll = Histogram::<u64>::new(3)?;
    let mut other = Histogram::<u64>::new(3)?;
    let overall = Instant::now();
    for _ in 0..checkpoints {
        for _ in 0..batch {
            let seq = writer.next_seq();
            let is_roll = seq != 0 && seq % capacity == 0;
            let start = Instant::now();
            writer.append(&record)?;
            let elapsed = elapsed_ns(start.elapsed())?;
            if is_roll {
                roll.record(elapsed)?;
            } else {
                other.record(elapsed)?;
            }
        }
        let start = Instant::now();
        writer.checkpoint()?;
        dirty.record(elapsed_ns(start.elapsed())?)?;
    }
    let seconds = overall.elapsed().as_secs_f64();
    let start = Instant::now();
    writer.checkpoint()?;
    empty.record(elapsed_ns(start.elapsed())?)?;
    let result = json!({"payload": payload_size, "batch": batch, "checkpoints": checkpoints,
        "segment_size": segment_size, "count": count, "seconds": seconds, "records_per_s": count as f64 / seconds,
        "checkpoint": histogram_json(&dirty), "empty_checkpoint": histogram_json(&empty),
        "roll_append": histogram_json(&roll), "other_append": histogram_json(&other)});
    if options.flag("--json") {
        println!("{result}");
    } else {
        println!(
            "payload={payload_size} B batch={batch} checkpoints={checkpoints} segment_size={segment_size} B records={count}"
        );
        for (label, key) in [
            ("Checkpoint", "checkpoint"),
            ("Empty checkpoint", "empty_checkpoint"),
            ("Roll append", "roll_append"),
            ("Other append", "other_append"),
        ] {
            print_histogram(label, &result[key]);
        }
        println!(
            "Overall: {} records/s (appends, instrumentation and dirty checkpoints)",
            result["records_per_s"]
        );
    }
    Ok(())
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP}");
        return;
    }
    if let Err(error) = run(&args) {
        eprintln!("tt-checkpoint: {error}");
        std::process::exit(1);
    }
}
