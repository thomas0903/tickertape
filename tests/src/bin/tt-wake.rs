//! Test helper for `tests/compat/v1.0.0-wake-ups.sh` (ADR 0053): one writer or reader process, built from this tree and
//! from the v1.0.0 tag (it uses only API that 1.0.0 has), so that the script can pair processes of both versions on one
//! journal.
//!
//! Usage:
//! - `tt-wake writer <dir> <records> <start_ms> <log>`: opens the journal, waits `start_ms`, then appends one record
//!   at a time, checkpoints it and pauses 23–63 ms; writes `<seq> <checkpoint done, Unix ns>` per record to `<log>`.
//! - `tt-wake reader <dir> durable|visible <records> <log>...`: a reader that blocks on the wake-up region at once
//!   (no spins, no yields, 1 ms polls without it), until it has `<records>` records or 60 s pass. Checks that the
//!   records come gap-free, then prints how many it got and, for every record, the delay from the writer's moment of
//!   reference (checkpoint done for a durable-only reader, the append for a visible one) to its delivery: the count of
//!   delays of 50 ms or more and the largest. On Linux it also prints its voluntary context switches.

use std::collections::HashMap;
use std::io::Write;
use std::time::{Duration, Instant};

use tickertape::{Backoff, NewRecord, Reader, ReaderConfig, Writer, WriterConfig, clock};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = &args[2];
    if args[1] == "writer" {
        let records: u64 = args[3].parse().expect("records");
        let start: u64 = args[4].parse().expect("start_ms");
        let mut log = std::fs::File::create(&args[5]).expect("log");
        let mut w = Writer::open(dir, WriterConfig::default()).expect("open writer");
        std::thread::sleep(Duration::from_millis(start));
        for i in 0..records {
            let seq = w
                .append(&NewRecord {
                    schema_id: 1,
                    venue_ts_ns: 0,
                    local_ts_ns: clock::realtime_ns(),
                    payload: &[0x5a; 64],
                })
                .expect("append");
            w.checkpoint().expect("checkpoint");
            writeln!(log, "{seq} {}", clock::realtime_ns()).expect("log");
            std::thread::sleep(Duration::from_millis(23 + i * 7 % 41));
        }
        return;
    }
    let durable = args[3] == "durable";
    let records: u64 = args[4].parse().expect("records");
    let mut backoff = Backoff::default();
    backoff.spins = 0;
    backoff.yields = 0;
    let mut r = Reader::open(
        dir,
        ReaderConfig::default()
            .durable_only(durable)
            .backoff(backoff),
    )
    .expect("open reader");
    let end = Instant::now() + Duration::from_secs(60);
    let mut got: Vec<(u64, u64, u64)> = Vec::new();
    while (got.len() as u64) < records && Instant::now() < end {
        if let Some(record) = r.next_timeout(Some(Duration::from_secs(5))).expect("read") {
            if let Some(last) = got.last() {
                assert_eq!(record.seq, last.0 + 1, "gap");
            }
            got.push((record.seq, record.local_ts_ns, clock::realtime_ns()));
        }
    }
    let mut done = HashMap::new();
    for log in &args[5..] {
        for line in std::fs::read_to_string(log).unwrap_or_default().lines() {
            // The writer may still be writing its last line: skip what does not parse (that record then counts from
            // its append, which only makes its delay longer).
            let mut f = line.split(' ').map(|x| x.parse::<u64>().ok());
            if let (Some(Some(seq)), Some(Some(at))) = (f.next(), f.next()) {
                done.insert(seq, at);
            }
        }
    }
    let delays: Vec<u64> = got
        .iter()
        .map(|&(seq, appended, at)| {
            let from = if durable {
                done.get(&seq).copied().unwrap_or(appended)
            } else {
                appended
            };
            at.saturating_sub(from)
        })
        .collect();
    let slow = delays.iter().filter(|&&d| d >= 50_000_000).count();
    let max = delays.iter().max().copied().unwrap_or(0);
    let switches = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("voluntary_ctxt_switches"))
                .and_then(|l| l.split_whitespace().nth(1).map(str::to_owned))
        })
        .unwrap_or_else(|| "-".into());
    println!(
        "got={} slow={slow} max_us={} switches={switches}",
        got.len(),
        max / 1000
    );
}
