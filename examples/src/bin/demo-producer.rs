//! Demo producer: writes synthetic market data (trades and order-book updates) to a tickertape journal.
//!
//! Usage: `demo-producer --dir <DIR> [--count N] [--rate RECORDS_PER_SEC] [--instruments N] [--seed S]`
//!
//! Prices follow a random walk per instrument. About one event in five is a trade. `venue_ts_ns` is a synthetic
//! exchange timestamp (a little before the local one); `local_ts_ns` is the local wall clock at write time. The
//! producer checkpoints (makes data durable) every 10,000 records and at the end.

use std::time::{Duration, Instant};

use tickertape::{NewRecord, Writer, WriterConfig};
use tickertape_examples::{Event, Rng, SCALE};

struct Args {
    dir: String,
    count: u64,
    rate: u64,
    instruments: u32,
    seed: u64,
}

fn parse() -> Args {
    let mut args = Args {
        dir: String::new(),
        count: 100_000,
        rate: 20_000,
        instruments: 3,
        seed: 1,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .unwrap_or_else(|| usage(&format!("{flag} needs a value")))
        };
        match flag.as_str() {
            "--dir" => args.dir = value(),
            "--count" => args.count = value().parse().unwrap_or_else(|_| usage("bad --count")),
            "--rate" => args.rate = value().parse().unwrap_or_else(|_| usage("bad --rate")),
            "--instruments" => {
                args.instruments = value()
                    .parse()
                    .unwrap_or_else(|_| usage("bad --instruments"))
            }
            "--seed" => args.seed = value().parse().unwrap_or_else(|_| usage("bad --seed")),
            "-h" | "--help" => usage(""),
            other => usage(&format!("unknown argument {other}")),
        }
    }
    if args.dir.is_empty() || args.rate == 0 || args.instruments == 0 {
        usage("--dir is required; --rate and --instruments must be positive");
    }
    args
}

fn usage(msg: &str) -> ! {
    if !msg.is_empty() {
        eprintln!("demo-producer: {msg}");
    }
    eprintln!(
        "usage: demo-producer --dir <DIR> [--count N] [--rate RECORDS_PER_SEC] [--instruments N] [--seed S]"
    );
    std::process::exit(if msg.is_empty() { 0 } else { 1 })
}

fn main() -> tickertape::Result<()> {
    let args = parse();
    let mut writer = Writer::open(&args.dir, WriterConfig::default())?;
    let mut rng = Rng::new(args.seed);
    let mut mid: Vec<i64> = (0..args.instruments)
        .map(|i| (100 + 50 * i64::from(i)) * SCALE as i64)
        .collect();
    let tick = SCALE as i64 / 100; // 0.01
    let mut trade_id = 0u64;
    let start = Instant::now();
    let interval = Duration::from_secs_f64(1.0 / args.rate as f64);

    for i in 0..args.count {
        // Pace to the requested rate.
        let due = start + interval.mul_f64(i as f64);
        while Instant::now() < due {
            std::hint::spin_loop();
        }
        let inst = rng.below(u64::from(args.instruments)) as usize;
        mid[inst] += (rng.below(3) as i64 - 1) * tick;
        mid[inst] = mid[inst].max(tick);
        let qty = (1 + rng.below(100) as i64) * SCALE as i64 / 10;
        let event = if rng.below(5) == 0 {
            trade_id += 1;
            Event::Trade {
                instrument_id: inst as u32,
                buy: rng.below(2) == 0,
                price: mid[inst],
                qty,
                trade_id,
            }
        } else {
            let bid = rng.below(2) == 0;
            let level = 1 + rng.below(5) as i64;
            let price = if bid {
                mid[inst] - level * tick
            } else {
                mid[inst] + level * tick
            };
            Event::Book {
                instrument_id: inst as u32,
                bid,
                delete: rng.below(10) == 0,
                price,
                qty,
            }
        };
        let local = tickertape::clock::realtime_ns();
        let payload = event.encode();
        writer.append(&NewRecord {
            schema_id: event.schema_id(),
            venue_ts_ns: local.saturating_sub(200_000 + rng.below(800_000)),
            local_ts_ns: local,
            payload: &payload,
        })?;
        if (i + 1) % 10_000 == 0 {
            writer.checkpoint()?;
        }
    }
    writer.checkpoint()?;
    let secs = start.elapsed().as_secs_f64();
    eprintln!(
        "demo-producer: wrote {} records ({trade_id} trades) in {secs:.2} s; journal ends at sequence {}",
        args.count,
        writer.next_seq()
    );
    Ok(())
}
