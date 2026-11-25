//! What the writer's latency histograms cost an append (ADR 0047): appends to count instructions over, and the parts of
//! a timed append by themselves. `benches/README.md` has the procedure. The lines marked `[not in the base]` name
//! things that a base commit without them does not have.

use std::hint::black_box;
use std::time::{Duration, Instant};

use tickertape::LatencyMode; // [not in the base]
use tickertape::latency::Recorder; // [not in the base]
use tickertape::{NewRecord, Writer, WriterConfig};
use tickertape_bench::{Options, Result, fresh_directory, segment_capacity};

const HELP: &str = "tt-append-cost: what the writer's latency histograms cost

Usage: tt-append-cost append --dir <DIR> --count <N> --payload <bytes>
                             [--mode off|rolls-and-checkpoints|all] [--prefault]
       tt-append-cost parts

append: opens a writer in a new or empty directory and appends N records into one 128 MiB segment (an error if they do
not fit), without helper threads unless --prefault. Run it under `perf stat -e instructions:u` or Callgrind with two
counts: the difference of the two totals, divided by the difference of the counts, is the cost of one append.
parts: nanoseconds per call of a clock pair, a histogram update (with the cost of making the values beside it) and the
50th percentile of an empty timed interval (the floor of every append sample).";

fn append(args: &[String]) -> Result<()> {
    let options = Options::parse(
        args,
        &["--dir", "--count", "--payload", "--mode"],
        &["--prefault"],
    )?;
    let dir = std::path::PathBuf::from(options.required("--dir")?);
    let count = options.number("--count", None)?;
    let payload = vec![0x5a; options.number("--payload", None)?.try_into()?];
    if count > segment_capacity(WriterConfig::DEFAULT_SEGMENT_SIZE, payload.len())? {
        return Err("the records do not fit in one segment".into());
    }
    fresh_directory(&dir)?;
    let config = WriterConfig::default().prefault(options.flag("--prefault"));
    let config = match options.optional("--mode").unwrap_or("off") {
        "off" => config,
        "rolls-and-checkpoints" => config.latency(LatencyMode::RollsAndCheckpoints), // [not in the base]
        "all" => config.latency(LatencyMode::All), // [not in the base]
        other => return Err(format!("unknown mode {other}").into()),
    };
    let mut writer = Writer::open(&dir, config)?;
    let record = NewRecord {
        schema_id: 1,
        venue_ts_ns: 0,
        local_ts_ns: 0,
        payload: &payload,
    };
    let started = Instant::now();
    for _ in 0..count {
        black_box(writer.append(black_box(&record))?);
    }
    let ns = started.elapsed().as_nanos() as f64 / count as f64;
    println!("{count} appends of {} B: {ns:.2} ns each", payload.len());
    Ok(())
}

/// The smallest of nine rounds, in nanoseconds per call.
fn per_call(mut call: impl FnMut(u64)) -> f64 {
    const CALLS: u64 = 5_000_000;
    (0..9)
        .map(|_| {
            let started = Instant::now();
            for i in 0..CALLS {
                call(i);
            }
            started.elapsed().as_nanos() as f64 / CALLS as f64
        })
        .fold(f64::INFINITY, f64::min)
}

/// Durations like those of appends, 40 to 103 ns, so that the bucket changes from call to call.
fn value(i: u64) -> Duration {
    Duration::from_nanos(40 + (i.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 58))
}

fn parts() {
    println!(
        "clock pair: {:.2} ns",
        per_call(|_| {
            black_box(black_box(Instant::now()).elapsed());
        })
    );
    println!(
        "value alone: {:.2} ns",
        per_call(|i| {
            black_box(value(i));
        })
    );
    let mut recorder = Recorder::new(); // [not in the base]
    let update = per_call(|i| recorder.record(black_box(value(i)))); // [not in the base]
    println!("value and histogram update: {update:.2} ns"); // [not in the base]
    let mut floor = Recorder::new(); // [not in the base]
    (0..5_000_000).for_each(|_| floor.record(Instant::now().elapsed())); // [not in the base]
    let floor = floor.view().snapshot(); // [not in the base]
    println!("empty interval: p50 {:?}", floor.value_at_quantile(0.5)); // [not in the base]
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("append") => append(&args[1..]),
        Some("parts") if args.len() == 1 => {
            parts();
            Ok(())
        }
        None | Some("--help" | "-h") => {
            println!("{HELP}");
            Ok(())
        }
        _ => Err("expected append or parts; see --help".into()),
    };
    if let Err(error) = result {
        eprintln!("tt-append-cost: {error}");
        std::process::exit(1);
    }
}
