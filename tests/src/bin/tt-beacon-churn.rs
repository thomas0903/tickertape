//! Test helper: creates, updates and drops reader beacons in a loop, contending with other processes for a few
//! names (ADR 0032 → "The beacon protocol"). Exits with 3 if a beacon it created is ever not published under its
//! name (a ghost), with 2 on any error other than the name being held by a live beacon, and sometimes aborts while
//! holding a beacon, as a crashed reader would.
//!
//! Usage: `tt-beacon-churn <journal> <beacons_dir> <name,name,...> <cycles> <seed> <abort_one_in> [go_file]`
//!
//! With `go_file`, it starts only once that file exists, so that several processes start contending together.

use std::path::PathBuf;
use std::time::Duration;

use tickertape::status::ReaderBeacon;
use tickertape::{Error, Reader, ReaderConfig};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let journal = PathBuf::from(&args[1]);
    let beacons = PathBuf::from(&args[2]);
    let names: Vec<&str> = args[3].split(',').collect();
    let cycles: u64 = args[4].parse().expect("cycles");
    let mut state: u64 = args[5].parse::<u64>().expect("seed") | 1;
    let abort_one_in: u64 = args[6].parse().expect("abort_one_in");
    let mut next = move || {
        // xorshift64
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let mut reader = Reader::open(&journal, ReaderConfig::default()).expect("open reader");
    if let Some(go) = args.get(7) {
        while !std::path::Path::new(go).exists() {
            std::thread::yield_now();
        }
    }
    let (mut created, mut in_use) = (0u64, 0u64);
    for _ in 0..cycles {
        let name = names[(next() % names.len() as u64) as usize];
        match ReaderBeacon::create_in(&beacons, name, &reader) {
            Ok(mut beacon) => {
                created += 1;
                if !beacon.is_published() {
                    eprintln!("ghost: {name} is not published after creation");
                    std::process::exit(3);
                }
                beacon.set_interval(Duration::ZERO);
                for _ in 0..next() % 4 {
                    let _ = reader.try_next().expect("read");
                    beacon.update(&reader).expect("update");
                    beacon.flush(&reader).expect("flush");
                }
                if !beacon.is_published() {
                    eprintln!("ghost: {name} was replaced while live");
                    std::process::exit(3);
                }
                if abort_one_in > 0 && next() % abort_one_in == 0 {
                    std::process::abort();
                }
                if next() % 8 == 0 {
                    std::thread::yield_now();
                }
                drop(beacon);
            }
            Err(Error::BeaconInUse { .. }) => in_use += 1,
            Err(e) => {
                eprintln!("create {name}: {e}");
                std::process::exit(2);
            }
        }
    }
    println!("{created} {in_use}");
}
