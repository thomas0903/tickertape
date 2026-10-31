//! Test helper: reads `count` records from a journal, live, checking that they are gap-free and that every payload
//! is what `tt-writer` wrote. Prints `<records> <digest>` on success; exits non-zero on any mismatch.
//!
//! Usage: `tt-reader <dir> <count> <max_payload> <safe|mapped> [timeout_secs] [delay_us_per_record]`

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tickertape::{Error, Reader, ReaderConfig};
use tickertape_tests::{bounded_len, digest_step, payload};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(&args[1]);
    let count: u64 = args[2].parse().expect("count");
    let max_payload: usize = args[3].parse().expect("max_payload");
    let mapped = match args[4].as_str() {
        "safe" => false,
        "mapped" => true,
        other => panic!("unknown mode {other}"),
    };
    let timeout = Duration::from_secs(args.get(5).map_or(300, |s| s.parse().expect("timeout")));
    let deadline = Instant::now() + timeout;
    let delay = Duration::from_micros(args.get(6).map_or(0, |s| s.parse().expect("delay")));

    // The writer may not have created the journal yet.
    let mut reader = loop {
        let opened = if mapped {
            // SAFETY: in these tests only the tickertape writer modifies the journal.
            unsafe { Reader::open_mapped(&dir, ReaderConfig::default()) }
        } else {
            Reader::open(&dir, ReaderConfig::default())
        };
        match opened {
            Ok(r) => break r,
            Err(Error::NotFound { .. } | Error::Io { .. }) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => panic!("open reader: {e}"),
        }
    };

    let mut digest = 0u64;
    let mut expected = 0u64;
    while expected < count {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            eprintln!("timeout after {expected} records");
            std::process::exit(3);
        }
        let Some(rec) = reader.next_timeout(Some(left)).expect("read") else {
            continue;
        };
        if rec.seq != expected {
            eprintln!("gap or reorder: expected {expected}, got {}", rec.seq);
            std::process::exit(2);
        }
        if rec.schema_id != (rec.seq % 7) as u32
            || rec.venue_ts_ns != rec.seq
            || rec.payload != payload(rec.seq, bounded_len(rec.seq, max_payload)).as_slice()
        {
            eprintln!("content mismatch at {}", rec.seq);
            std::process::exit(2);
        }
        digest = digest_step(digest, rec.seq, rec.schema_id, rec.payload);
        expected += 1;
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
    }
    println!("{expected} {digest:016x}");
}
