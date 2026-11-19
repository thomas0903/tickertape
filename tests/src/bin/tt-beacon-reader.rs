//! Test helper: a reader that publishes a beacon (ADR 0032). Reads `count` records, flushes its beacon, creates
//! `ready_file`, then either holds (sleeps until killed) or aborts without running destructors, as a crashed reader
//! would.
//!
//! Usage: `tt-beacon-reader <dir> <name> <count> <hold|abort> <ready_file>`

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tickertape::status::ReaderBeacon;
use tickertape::{Error, Reader, ReaderConfig};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(&args[1]);
    let name = &args[2];
    let count: u64 = args[3].parse().expect("count");
    let then = args[4].as_str();
    let ready = PathBuf::from(&args[5]);
    let deadline = Instant::now() + Duration::from_secs(120);

    let mut reader = loop {
        match Reader::open(&dir, ReaderConfig::default()) {
            Ok(r) => break r,
            Err(Error::NotFound { .. } | Error::Io { .. }) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => panic!("open reader: {e}"),
        }
    };
    let mut beacon = ReaderBeacon::create(&dir, name, &reader).expect("create beacon");
    beacon.set_interval(Duration::from_millis(10));
    while reader.position() < count {
        assert!(
            Instant::now() < deadline,
            "timed out at {}",
            reader.position()
        );
        reader
            .next_timeout(Some(Duration::from_millis(100)))
            .expect("read");
        beacon.update(&reader).expect("update beacon");
    }
    beacon.flush(&reader).expect("flush beacon");
    std::fs::write(&ready, b"").expect("ready file");
    match then {
        "hold" => loop {
            std::thread::sleep(Duration::from_secs(1));
        },
        "abort" => std::process::abort(),
        other => panic!("unknown mode {other}"),
    }
}
