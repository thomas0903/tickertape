//! Demo consumer: follows a tickertape journal live from another process and prints a summary every second.
//!
//! Usage: `demo-consumer --dir <DIR> [--count N] [--idle-secs S]`
//!
//! It waits for the journal to appear, reads from the first record, decodes the demo market-data events, and
//! reports records per second, trades, the last trade price per instrument, and how far behind the writer it is
//! (local write time to read time, on the same host's wall clock). It exits after `--count` records, or after
//! `--idle-secs` seconds without new records (default 5).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use tickertape::{Error, Reader, ReaderConfig};
use tickertape_examples::{Event, SCALE};

fn main() -> tickertape::Result<()> {
    let mut dir = String::new();
    let mut count = u64::MAX;
    let mut idle = 5u64;
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let value = it.next();
        match (flag.as_str(), value) {
            ("--dir", Some(v)) => dir = v,
            ("--count", Some(v)) => count = v.parse().expect("--count"),
            ("--idle-secs", Some(v)) => idle = v.parse().expect("--idle-secs"),
            _ => {
                eprintln!("usage: demo-consumer --dir <DIR> [--count N] [--idle-secs S]");
                std::process::exit(1);
            }
        }
    }

    // The producer may not have created the journal yet.
    let started = Instant::now();
    let mut reader = loop {
        match Reader::open(&dir, ReaderConfig::default()) {
            Ok(r) => break r,
            Err(Error::NotFound { .. } | Error::Io { .. })
                if started.elapsed() < Duration::from_secs(idle) =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    };

    let (mut total, mut trades, mut books, mut window) = (0u64, 0u64, 0u64, 0u64);
    let mut last_price: BTreeMap<u32, f64> = BTreeMap::new();
    let mut max_lag_us = 0u64;
    let mut tick = Instant::now();
    while total < count {
        let Some(record) = reader.next_timeout(Some(Duration::from_secs(idle)))? else {
            eprintln!("demo-consumer: no new records for {idle} s, stopping");
            break;
        };
        let lag = tickertape::clock::realtime_ns().saturating_sub(record.local_ts_ns) / 1_000;
        max_lag_us = max_lag_us.max(lag);
        match Event::decode(record.schema_id, record.payload) {
            Some(Event::Trade {
                instrument_id,
                price,
                ..
            }) => {
                trades += 1;
                last_price.insert(instrument_id, price as f64 / SCALE);
            }
            Some(Event::Book { .. }) => books += 1,
            None => {}
        }
        total += 1;
        window += 1;
        if tick.elapsed() >= Duration::from_secs(1) {
            let prices: Vec<String> = last_price
                .iter()
                .map(|(i, p)| format!("#{i}={p:.2}"))
                .collect();
            println!(
                "seq={} rate={window}/s trades={trades} book={books} max_lag={max_lag_us}us last=[{}]",
                record.seq,
                prices.join(" ")
            );
            window = 0;
            max_lag_us = 0;
            tick = Instant::now();
        }
    }
    println!("demo-consumer: read {total} records ({trades} trades, {books} book updates)");
    Ok(())
}
